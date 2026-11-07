//! Clipboard history: what the user copies is kept, newest first, and the
//! tray menu gives it back with a click.
//!
//! macOS posts no notification when the pasteboard changes, so a worker polls
//! `changeCount` and reads the pasteboard only when it moved. Copies live in
//! their own `clipboard.db`, apart from the transcript history and its
//! migrations, readable only by the user, and never leave the machine.
//!
//! Never kept: what a password manager or Handy's own paste marks as
//! concealed or transient, text that looks like a key, token or password, and
//! dictations, which the tray already lists.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::time::Duration;

use log::{debug, error, info};
use rusqlite::{params, Connection};
use tauri::{AppHandle, Manager};

use crate::managers::history::HistoryManager;

/// How many copies are kept; older ones are dropped.
const KEEP: usize = 50;
/// How many copies the tray menu lists.
pub const TRAY_ITEMS: usize = 10;
/// Longer copies (a whole file, a log dump) aren't kept.
const MAX_CHARS: usize = 20_000;
/// Dictations this recent are left out: the tray lists them already.
const RECENT_DICTATIONS: usize = 5;

/// Mirrors the `clipboard_history` setting, so the worker doesn't read the
/// settings store twice a second.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// One remembered copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copied {
    pub id: i64,
    pub text: String,
    /// Unix seconds.
    pub copied_at: i64,
}

/// Reads the setting and, on macOS, starts watching the pasteboard.
pub fn start(app: &AppHandle) {
    ENABLED.store(
        crate::settings::get_settings(app).clipboard_history,
        Ordering::Relaxed,
    );
    #[cfg(target_os = "macos")]
    {
        let app = app.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("clipboard-history".into())
            .spawn(move || macos::watch(app))
        {
            error!("Failed to start the clipboard history worker: {err}");
        }
    }
}

/// The newest copies, for the tray. Empty while the setting is off.
pub fn recent(app: &AppHandle, limit: usize) -> Vec<Copied> {
    if !ENABLED.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let Some(path) = db_path(app) else {
        return Vec::new();
    };
    match open(&path).and_then(|conn| recent_from(&conn, limit)) {
        Ok(copies) => copies,
        Err(err) => {
            error!("Failed to read the clipboard history: {err}");
            Vec::new()
        }
    }
}

/// Forgets every copy and refreshes the tray.
pub fn clear(app: &AppHandle) {
    if let Some(path) = db_path(app) {
        if let Err(err) = open(&path).and_then(|conn| clear_db(&conn)) {
            error!("Failed to clear the clipboard history: {err}");
        } else {
            info!("Cleared the clipboard history.");
        }
    }
    crate::tray::update_tray_menu(app);
}

#[tauri::command]
#[specta::specta]
pub fn change_clipboard_history_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.clipboard_history = enabled;
    crate::settings::write_settings(&app, settings);
    ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        crate::tray::update_tray_menu(&app);
    } else {
        // Off means forgotten, not just hidden.
        clear(&app);
    }
    Ok(())
}

/// Saves a copy the worker read, unless it's one to leave out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn remember(app: &AppHandle, text: &str) {
    if !worth_keeping(text) || is_recent_dictation(app, text) {
        return;
    }
    let Some(path) = db_path(app) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    match open(&path).and_then(|conn| add(&conn, text, now)) {
        Ok(()) => {
            debug!("Kept a clipboard copy ({} chars).", text.chars().count());
            crate::tray::update_tray_menu(app);
        }
        Err(err) => error!("Failed to save a clipboard copy: {err}"),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn worth_keeping(text: &str) -> bool {
    let trimmed = text.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_CHARS && !looks_secret(trimmed)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_recent_dictation(app: &AppHandle, text: &str) -> bool {
    let Some(history) = app.try_state::<Arc<HistoryManager>>() else {
        return false;
    };
    let text = text.trim();
    history
        .get_recent_completed_entries(RECENT_DICTATIONS)
        .map(|entries| {
            entries.iter().any(|entry| {
                entry.transcription_text.trim() == text
                    || entry.post_processed_text.as_deref().map(str::trim) == Some(text)
            })
        })
        .unwrap_or(false)
}

/// Key prefixes of common services; a word starting with one is a secret.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "pk_live_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
    "eyJ",
    "npm_",
    "pypi-",
    "shpat_",
    "hf_",
];
/// Words that make `name = value` or `name: value` a credential line.
const SECRET_NAMES: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "senha",
    "credential",
];
/// URL query parameters that carry credentials.
const SECRET_PARAMS: &[&str] = &[
    "token=",
    "key=",
    "secret=",
    "password=",
    "sig=",
    "signature=",
    "code=",
];

/// True for text that shouldn't be remembered: keys, tokens, passwords.
/// Errs on the side of skipping; a skipped copy still pastes as usual.
pub(crate) fn looks_secret(text: &str) -> bool {
    if text.contains("-----BEGIN") {
        return true;
    }
    let credential_line = text.lines().any(|line| {
        line.split_once(|c| c == '=' || c == ':')
            .is_some_and(|(name, value)| {
                let name = name.trim().to_lowercase();
                !value.trim().is_empty()
                    && !value.starts_with("//")
                    && name.len() <= 32
                    && SECRET_NAMES.iter().any(|word| name.contains(word))
            })
    });
    if credential_line {
        return true;
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.iter().any(|word| is_token(word)) {
        return true;
    }
    match words.as_slice() {
        [word] => looks_like_password(word),
        _ => false,
    }
}

/// A key-shaped word: a known prefix, a long run of key characters with
/// letters and digits, or a URL carrying credentials.
fn is_token(word: &str) -> bool {
    if word.starts_with("http://") || word.starts_with("https://") {
        let lower = word.to_lowercase();
        return lower.split_once('?').is_some_and(|(_, query)| {
            query
                .split('&')
                .any(|pair| SECRET_PARAMS.iter().any(|p| pair.contains(p)))
        });
    }
    if word.len() >= 16 && SECRET_PREFIXES.iter().any(|p| word.starts_with(p)) {
        return true;
    }
    word.len() >= 24
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_+/=.".contains(c))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
        && !word.contains("//")
        && !word.starts_with('/')
        && !word.starts_with('~')
}

/// A lone word mixing cases and digits, or symbols with two other kinds of
/// character. Paths and addresses don't count.
fn looks_like_password(word: &str) -> bool {
    let len = word.chars().count();
    if !(8..=64).contains(&len) || word.contains('/') || word.contains('@') {
        return false;
    }
    let lower = word.chars().any(|c| c.is_lowercase());
    let upper = word.chars().any(|c| c.is_uppercase());
    let digit = word.chars().any(|c| c.is_ascii_digit());
    let symbol = word.chars().any(|c| "!#$%^&*()+=?{}[]<>|~".contains(c));
    let kinds = [lower, upper, digit].iter().filter(|kind| **kind).count();
    (lower && upper && digit) || (symbol && kinds >= 2)
}

fn db_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|dir| dir.join("clipboard.db"))
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS clipboard_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            text TEXT NOT NULL,
            copied_at INTEGER NOT NULL
        );",
    )
}

/// Adds `text` as the newest copy. Copying it again moves it to the top, and
/// only the newest [`KEEP`] stay.
fn add(conn: &Connection, text: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM clipboard_history WHERE text = ?1", [text])?;
    conn.execute(
        "INSERT INTO clipboard_history (text, copied_at) VALUES (?1, ?2)",
        params![text, now],
    )?;
    conn.execute(
        "DELETE FROM clipboard_history WHERE id NOT IN
            (SELECT id FROM clipboard_history ORDER BY id DESC LIMIT ?1)",
        [KEEP as i64],
    )?;
    Ok(())
}

fn recent_from(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<Copied>> {
    let mut stmt = conn
        .prepare("SELECT id, text, copied_at FROM clipboard_history ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(Copied {
            id: row.get(0)?,
            text: row.get(1)?,
            copied_at: row.get(2)?,
        })
    })?;
    rows.collect()
}

fn clear_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM clipboard_history", [])?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

    /// How often `changeCount` is checked. Reading it is one integer.
    const POLL: Duration = Duration::from_millis(500);
    /// Markers apps put on copies that shouldn't be remembered
    /// (nspasteboard.org). Handy's own reliable paste sets the first two.
    const SKIP_TYPES: &[&str] = &[
        "org.nspasteboard.TransientType",
        "org.nspasteboard.ConcealedType",
        "org.nspasteboard.AutoGeneratedType",
        "de.petermaurer.TransientPasteboardType",
        "com.agilebits.onepassword",
    ];

    pub(super) fn watch(app: AppHandle) {
        let mut last = NSPasteboard::generalPasteboard().changeCount();
        loop {
            std::thread::sleep(POLL);
            let count = NSPasteboard::generalPasteboard().changeCount();
            if count == last {
                continue;
            }
            last = count;
            if !ENABLED.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(text) = read_on_main(&app) {
                remember(&app, &text);
            }
        }
    }

    /// AppKit serves promised pasteboard data on the main run loop, so the
    /// read happens there.
    fn read_on_main(app: &AppHandle) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(read_text());
        })
        .ok()?;
        rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    fn read_text() -> Option<String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let types = pasteboard.types()?;
        let marked = types
            .iter()
            .any(|kind| SKIP_TYPES.contains(&kind.to_string().as_str()));
        if marked {
            return None;
        }
        pasteboard
            .stringForType(unsafe { NSPasteboardTypeString })
            .map(|text| text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_tokens_are_secret() {
        for text in [
            "sk-proj-4f9a8b7c6d5e4f3a2b1c",
            "ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "OPENAI_API_KEY=sk-abc123def456ghi789",
            "Senha: hunter2",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc",
            "https://example.com/callback?access_token=abc123",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
            "Xk9#mP2q",
            "Tr0ub4dor",
        ] {
            assert!(looks_secret(text), "{text} should be secret");
        }
    }

    #[test]
    fn everyday_copies_are_kept() {
        for text in [
            "Bom dia, tudo bem?",
            "Edex",
            "https://github.com/LimaJoota/Handy",
            "/Users/someone/Documents/file.txt",
            "someone@example.com",
            "LimaJoota/Handy",
            "v0.10.0-lj.3",
            "947bf4a6",
            "feat/clipboard-history",
            "The meeting is at 10:30, room B.",
            "url: https://example.com",
        ] {
            assert!(!looks_secret(text), "{text} should be kept");
        }
    }

    #[test]
    fn blank_and_huge_copies_are_skipped() {
        assert!(!worth_keeping("   \n "));
        assert!(!worth_keeping(&"a ".repeat(MAX_CHARS)));
        assert!(worth_keeping("Oi"));
    }

    #[test]
    fn copying_again_moves_to_the_top_and_old_ones_drop() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        add(&conn, "first", 1).unwrap();
        add(&conn, "second", 2).unwrap();
        add(&conn, "first", 3).unwrap();
        let texts: Vec<String> = recent_from(&conn, 10)
            .unwrap()
            .into_iter()
            .map(|copy| copy.text)
            .collect();
        assert_eq!(texts, ["first", "second"]);

        for n in 0..KEEP + 5 {
            add(&conn, &format!("copy {n}"), 10 + n as i64).unwrap();
        }
        let all = recent_from(&conn, KEEP * 2).unwrap();
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0].text, format!("copy {}", KEEP + 4));

        clear_db(&conn).unwrap();
        assert!(recent_from(&conn, 10).unwrap().is_empty());
    }
}
