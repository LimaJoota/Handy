//! Clipboard history: what the user copies is kept, newest first, and the
//! tray menu gives it back with a click.
//!
//! macOS posts no notification when the pasteboard changes, so a worker polls
//! `changeCount` and reads the pasteboard only when it moved. Copies live in
//! their own `clipboard.db`, apart from the transcript history and its
//! migrations, and never leave the machine.
//!
//! Passwords and keys are kept like anything else (copying a few and pasting
//! them is the point), so every copy is encrypted with AES-256-GCM under a key
//! held in the login Keychain; the file alone reveals nothing. The menu shows
//! only the start of what looks like a secret.
//!
//! Never kept: Handy's own dictations, which the tray already lists, and what
//! an app marks as transient or auto-generated.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
#[cfg(target_os = "macos")]
use std::time::Duration;

use log::{debug, error, info};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection};
use tauri::{AppHandle, Manager};

use crate::managers::history::HistoryManager;

/// Most copies the history may keep, whatever the setting says.
const MAX_KEEP: usize = 1000;
/// Copies listed directly in the pointer menu, with keys 1–9 and 0; the rest
/// go in submenus of the same size.
const MENU_GROUP: usize = 10;
/// Time for the menu to close and focus to settle before pasting.
#[cfg(target_os = "macos")]
const PICK_SETTLE: Duration = Duration::from_millis(120);
/// How many copies the tray menu lists.
pub const TRAY_ITEMS: usize = 10;
/// Longer copies (a whole file, a log dump) aren't kept.
const MAX_CHARS: usize = 20_000;
/// Dictations this recent are left out: the tray lists them already.
const RECENT_DICTATIONS: usize = 5;

/// Mirrors the `clipboard_history` setting, so the worker doesn't read the
/// settings store twice a second.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Set once the Keychain hands over the key; nothing is read or saved before.
static VAULT: OnceLock<Vault> = OnceLock::new();
/// Visible characters of a copy that looks like a secret, in the menu.
const SECRET_SHOWN: usize = 3;

/// Encrypts copies and fingerprints them, so a repeated copy can be found
/// without storing its text.
struct Vault {
    seal: LessSafeKey,
    mac: hmac::Key,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Vault {
    /// Key bytes as stored in the Keychain: 32 for AES-256-GCM, 32 for HMAC.
    const KEY_LEN: usize = 64;

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::KEY_LEN {
            return None;
        }
        let seal = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &bytes[..32]).ok()?);
        let mac = hmac::Key::new(hmac::HMAC_SHA256, &bytes[32..]);
        Some(Self { seal, mac })
    }

    /// `nonce || ciphertext || tag`.
    fn seal(&self, text: &str) -> Option<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).ok()?;
        let mut sealed = text.as_bytes().to_vec();
        self.seal
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut sealed,
            )
            .ok()?;
        let mut blob = nonce.to_vec();
        blob.append(&mut sealed);
        Some(blob)
    }

    fn open(&self, blob: &[u8]) -> Option<String> {
        if blob.len() < NONCE_LEN {
            return None;
        }
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut sealed = sealed.to_vec();
        let text = self
            .seal
            .open_in_place(nonce, Aad::empty(), &mut sealed)
            .ok()?;
        String::from_utf8(text.to_vec()).ok()
    }

    fn digest(&self, text: &str) -> Vec<u8> {
        hmac::sign(&self.mac, text.as_bytes()).as_ref().to_vec()
    }
}

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
    let (Some(vault), Some(path)) = (VAULT.get(), db_path(app)) else {
        return Vec::new();
    };
    match open(&path).and_then(|conn| recent_from(&conn, vault, limit)) {
        Ok(copies) => copies,
        Err(err) => {
            error!("Failed to read the clipboard history: {err}");
            Vec::new()
        }
    }
}

/// How many copies to keep, from the setting.
fn keep(app: &AppHandle) -> usize {
    crate::settings::get_settings(app)
        .clipboard_history_limit
        .clamp(1, MAX_KEEP)
}

/// Opens the history at the pointer. Picking a copy (or pressing its number)
/// pastes it into the app in front.
pub fn show_menu(app: &AppHandle) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let copies = recent(app, keep(app));
        let locale = crate::settings::get_settings(app).app_language;
        if let Err(err) = app.run_on_main_thread(move || macos::pop_up(&copies, locale)) {
            error!("Failed to open the clipboard history menu: {err}");
        }
    }
}

/// Pastes the copy picked in the pointer menu.
pub fn paste_item(app: &AppHandle, id: i64) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(text) = get(&app, id) else {
            error!("Clipboard history item {id} is gone");
            return;
        };
        #[cfg(target_os = "macos")]
        std::thread::sleep(PICK_SETTLE);
        if let Err(err) = crate::clipboard::paste_from_history(&text, &app) {
            error!("Failed to paste a clipboard history item: {err}");
        }
    });
}

/// One copy by id.
fn get(app: &AppHandle, id: i64) -> Option<String> {
    let (vault, path) = (VAULT.get()?, db_path(app)?);
    let conn = open(&path).ok()?;
    let data: Vec<u8> = conn
        .query_row("SELECT data FROM clips WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .ok()?;
    vault.open(&data)
}

#[tauri::command]
#[specta::specta]
pub fn change_clipboard_history_limit_setting(app: AppHandle, limit: usize) -> Result<(), String> {
    let mut settings = crate::settings::get_settings(&app);
    settings.clipboard_history_limit = limit.clamp(1, MAX_KEEP);
    crate::settings::write_settings(&app, settings);
    Ok(())
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
    let (Some(vault), Some(path)) = (VAULT.get(), db_path(app)) else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    match open(&path).and_then(|conn| add(&conn, vault, text, now, keep(app))) {
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
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_CHARS
}

/// What the menu shows for a copy: secrets only by their first characters.
pub fn shown(text: &str) -> Cow<'_, str> {
    if looks_secret(text.trim()) {
        let start: String = text.trim().chars().take(SECRET_SHOWN).collect();
        Cow::Owned(format!("{start}••••••"))
    } else {
        Cow::Borrowed(text)
    }
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

/// True for text that looks like a key, token or password, which the menu
/// doesn't spell out.
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
    // The first prototype kept plain text in `clipboard_history`: drop it and
    // wipe its pages from the file.
    let plain: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'clipboard_history'",
        [],
        |row| row.get(0),
    )?;
    if plain > 0 {
        conn.execute_batch("DROP TABLE clipboard_history; VACUUM;")?;
    }
    conn.execute_batch(
        "PRAGMA secure_delete = ON;
        CREATE TABLE IF NOT EXISTS clips (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            digest BLOB NOT NULL,
            data BLOB NOT NULL,
            copied_at INTEGER NOT NULL
        );",
    )
}

/// Adds `text` as the newest copy, encrypted. Copying it again moves it to the
/// top, and only the newest `keep` stay.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn add(
    conn: &Connection,
    vault: &Vault,
    text: &str,
    now: i64,
    keep: usize,
) -> rusqlite::Result<()> {
    let Some(data) = vault.seal(text) else {
        error!("Failed to encrypt a clipboard copy");
        return Ok(());
    };
    let digest = vault.digest(text);
    conn.execute("DELETE FROM clips WHERE digest = ?1", [&digest])?;
    conn.execute(
        "INSERT INTO clips (digest, data, copied_at) VALUES (?1, ?2, ?3)",
        params![digest, data, now],
    )?;
    conn.execute(
        "DELETE FROM clips WHERE id NOT IN (SELECT id FROM clips ORDER BY id DESC LIMIT ?1)",
        [keep as i64],
    )?;
    Ok(())
}

/// Newest first. Rows the key can't open (from a key since lost) are skipped.
fn recent_from(conn: &Connection, vault: &Vault, limit: usize) -> rusqlite::Result<Vec<Copied>> {
    let mut stmt =
        conn.prepare("SELECT id, data, copied_at FROM clips ORDER BY id DESC LIMIT ?1")?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut copies = Vec::new();
    for row in rows {
        let (id, data, copied_at) = row?;
        if let Some(text) = vault.open(&data) {
            copies.push(Copied {
                id,
                text,
                copied_at,
            });
        }
    }
    Ok(copies)
}

fn clear_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM clips", [])?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};

    /// How often `changeCount` is checked. Reading it is one integer.
    const POLL: Duration = Duration::from_millis(500);
    /// Markers for copies that were never the user's (nspasteboard.org).
    /// Handy's own reliable paste sets the first. Concealed copies, such as a
    /// password manager's, are kept: the history is encrypted.
    const SKIP_TYPES: &[&str] = &[
        "org.nspasteboard.TransientType",
        "org.nspasteboard.AutoGeneratedType",
        "de.petermaurer.TransientPasteboardType",
    ];
    /// The login Keychain item holding the key.
    const KEYCHAIN_SERVICE: &str = "Handy Clipboard History";
    const KEYCHAIN_ACCOUNT: &str = "encryption-key";
    /// `errSecItemNotFound`.
    const ITEM_NOT_FOUND: i32 = -25300;
    /// Set when the Keychain refused, so a denied prompt isn't repeated on
    /// every copy.
    static KEY_REFUSED: AtomicBool = AtomicBool::new(false);

    pub(super) fn watch(app: AppHandle) {
        let mut last = NSPasteboard::generalPasteboard().changeCount();
        if ENABLED.load(Ordering::Relaxed) {
            unlock(&app);
        }
        loop {
            std::thread::sleep(POLL);
            let count = NSPasteboard::generalPasteboard().changeCount();
            if count == last {
                continue;
            }
            last = count;
            if !ENABLED.load(Ordering::Relaxed) || !unlock(&app) {
                continue;
            }
            if let Some(text) = read_on_main(&app) {
                remember(&app, &text);
            }
        }
    }

    /// Shows the history as a native menu at the pointer: the newest
    /// [`MENU_GROUP`] with keys 1–9 and 0, older ones in submenus. AppKit runs
    /// the menu until it closes; the pick arrives as a menu event.
    pub(super) fn pop_up(copies: &[Copied], locale: String) {
        if let Err(err) = build_and_pop_up(copies, locale) {
            error!("Failed to show the clipboard history menu: {err}");
        }
    }

    fn build_and_pop_up(copies: &[Copied], locale: String) -> muda::Result<()> {
        use muda::accelerator::{Accelerator, Code};
        use muda::{ContextMenu, Menu, MenuItem, PredefinedMenuItem, Submenu};
        use objc2_app_kit::{NSEvent, NSMenu};

        const KEYS: [Code; MENU_GROUP] = [
            Code::Digit1,
            Code::Digit2,
            Code::Digit3,
            Code::Digit4,
            Code::Digit5,
            Code::Digit6,
            Code::Digit7,
            Code::Digit8,
            Code::Digit9,
            Code::Digit0,
        ];
        let strings = crate::tray_i18n::get_tray_translations(Some(locale));
        let item = |copy: &Copied, key: Option<Code>| {
            MenuItem::with_id(
                format!("paste_copied:{}", copy.id),
                crate::tray::menu_label(&shown(&copy.text), copy.copied_at),
                true,
                key.map(|code| Accelerator::new(None, code)),
            )
        };

        let menu = Menu::new();
        menu.append(&MenuItem::with_id(
            "copied_recently",
            &strings.copied_recently,
            false,
            None,
        ))?;
        let (first, rest) = copies.split_at(copies.len().min(MENU_GROUP));
        for (copy, key) in first.iter().zip(KEYS) {
            menu.append(&item(copy, Some(key)))?;
        }
        for (group, chunk) in rest.chunks(MENU_GROUP).enumerate() {
            let start = MENU_GROUP * (group + 1) + 1;
            let submenu = Submenu::new(format!("{start}–{}", start + chunk.len() - 1), true);
            for copy in chunk {
                submenu.append(&item(copy, None))?;
            }
            menu.append(&submenu)?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::with_id(
            "clear_copied",
            &strings.clear_copied,
            !copies.is_empty(),
            None,
        ))?;

        // SAFETY: muda hands out its live NSMenu, kept alive by `menu` for the
        // whole (blocking) popup; this runs on the main thread.
        let ns_menu = unsafe { &*(menu.ns_menu() as *const NSMenu) };
        let location = NSEvent::mouseLocation();
        ns_menu.popUpMenuPositioningItem_atLocation_inView(None, location, None);
        Ok(())
    }

    /// Gets the key from the Keychain, creating it on first use. A new key
    /// can't open older copies, so they're dropped.
    fn unlock(app: &AppHandle) -> bool {
        if VAULT.get().is_some() {
            return true;
        }
        if KEY_REFUSED.load(Ordering::Relaxed) {
            return false;
        }
        let Some((bytes, created)) = load_or_create_key() else {
            KEY_REFUSED.store(true, Ordering::Relaxed);
            return false;
        };
        let Some(vault) = Vault::from_bytes(&bytes) else {
            error!("The clipboard history key in the Keychain has the wrong size");
            KEY_REFUSED.store(true, Ordering::Relaxed);
            return false;
        };
        if created {
            if let Some(path) = db_path(app) {
                if let Err(err) = open(&path).and_then(|conn| clear_db(&conn)) {
                    error!("Failed to drop copies sealed with an older key: {err}");
                }
            }
        }
        let _ = VAULT.set(vault);
        crate::tray::update_tray_menu(app);
        true
    }

    fn load_or_create_key() -> Option<(Vec<u8>, bool)> {
        use security_framework::passwords::{get_generic_password, set_generic_password};
        match get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
            Ok(bytes) => Some((bytes, false)),
            Err(err) if err.code() == ITEM_NOT_FOUND => {
                let mut bytes = vec![0u8; Vault::KEY_LEN];
                SystemRandom::new().fill(&mut bytes).ok()?;
                match set_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, &bytes) {
                    Ok(()) => {
                        info!("Created the clipboard history key in the Keychain.");
                        Some((bytes, true))
                    }
                    Err(err) => {
                        error!("Failed to save the clipboard history key: {err}");
                        None
                    }
                }
            }
            Err(err) => {
                error!("Keychain refused the clipboard history key: {err}");
                None
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

    const KEEP: usize = 50;

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
    fn blank_and_huge_copies_are_skipped_but_secrets_kept() {
        assert!(!worth_keeping("   \n "));
        assert!(!worth_keeping(&"a ".repeat(MAX_CHARS)));
        assert!(worth_keeping("Oi"));
        assert!(worth_keeping("Xk9#mP2q"));
        assert!(worth_keeping("ghp_16C7e42F292c6912E7710c838347Ae178B4a"));
    }

    #[test]
    fn the_menu_shows_only_the_start_of_a_secret() {
        assert_eq!(shown("Xk9#mP2q"), "Xk9••••••");
        assert_eq!(shown("Bom dia, tudo bem?"), "Bom dia, tudo bem?");
    }

    fn vault() -> Vault {
        Vault::from_bytes(&[7u8; Vault::KEY_LEN]).unwrap()
    }

    #[test]
    fn copies_are_stored_encrypted() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        add(&conn, &vault(), "my secret password", 1, KEEP).unwrap();
        let data: Vec<u8> = conn
            .query_row("SELECT data FROM clips", [], |row| row.get(0))
            .unwrap();
        assert!(!data.windows(6).any(|w| w == b"secret"));
        assert_eq!(
            recent_from(&conn, &vault(), 1).unwrap()[0].text,
            "my secret password"
        );

        let other = Vault::from_bytes(&[8u8; Vault::KEY_LEN]).unwrap();
        assert!(recent_from(&conn, &other, 1).unwrap().is_empty());
    }

    #[test]
    fn the_plain_text_prototype_table_is_dropped() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE clipboard_history (id INTEGER PRIMARY KEY, text TEXT, copied_at INTEGER);
            INSERT INTO clipboard_history (text, copied_at) VALUES ('old', 1);",
        )
        .unwrap();
        init(&conn).unwrap();
        let plain: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'clipboard_history'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(plain, 0);
    }

    #[test]
    fn copying_again_moves_to_the_top_and_old_ones_drop() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let v = vault();
        add(&conn, &v, "first", 1, KEEP).unwrap();
        add(&conn, &v, "second", 2, KEEP).unwrap();
        add(&conn, &v, "first", 3, KEEP).unwrap();
        let texts: Vec<String> = recent_from(&conn, &v, 10)
            .unwrap()
            .into_iter()
            .map(|copy| copy.text)
            .collect();
        assert_eq!(texts, ["first", "second"]);

        for n in 0..KEEP + 5 {
            add(&conn, &v, &format!("copy {n}"), 10 + n as i64, KEEP).unwrap();
        }
        let all = recent_from(&conn, &v, KEEP * 2).unwrap();
        assert_eq!(all.len(), KEEP);
        assert_eq!(all[0].text, format!("copy {}", KEEP + 4));

        clear_db(&conn).unwrap();
        assert!(recent_from(&conn, &v, 10).unwrap().is_empty());
    }
}
