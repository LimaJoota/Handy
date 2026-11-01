import { commands, type AccentColor } from "@/bindings";

/**
 * Accent color handling, alongside `theme.ts`.
 *
 * Pink is Handy's default palette. Any other choice sets `data-accent` on the
 * document root, whose overrides in `theme.css` swap the palette's sources;
 * `custom` also passes the picked color in as `--accent-custom`.
 *
 * Like the theme, the choice is persisted in `AppSettings` (source of truth)
 * and mirrored to localStorage so it can be applied synchronously on boot.
 */

export const ACCENT_STORAGE_KEY = "handy.accent";

export const ACCENT_OPTIONS: AccentColor[] = [
  "pink",
  "system",
  "purple",
  "custom",
];

export const DEFAULT_ACCENT_CUSTOM_COLOR = "#8b5cf6";

const isAccent = (value: unknown): value is AccentColor =>
  typeof value === "string" && (ACCENT_OPTIONS as string[]).includes(value);

/** The CSS `AccentColor` keyword, which only some webviews resolve to the OS. */
export const supportsSystemAccent = (): boolean =>
  typeof CSS !== "undefined" && CSS.supports("color", "AccentColor");

/** Apply an accent to the document root and remember it for the next launch. */
export const applyAccent = (
  accent: AccentColor,
  customColor: string = DEFAULT_ACCENT_CUSTOM_COLOR,
): void => {
  const root = document.documentElement;
  if (accent === "pink" || (accent === "system" && !supportsSystemAccent())) {
    delete root.dataset.accent;
  } else {
    root.dataset.accent = accent;
  }
  root.style.setProperty("--accent-custom", customColor);
  try {
    localStorage.setItem(
      ACCENT_STORAGE_KEY,
      JSON.stringify({ accent, customColor }),
    );
  } catch {
    // Same as the theme: only costs a one-frame flash on boot.
  }
};

/** Read the last-applied accent for synchronous boot-time application. */
export const getStoredAccent = (): [AccentColor, string] => {
  try {
    const stored = JSON.parse(localStorage.getItem(ACCENT_STORAGE_KEY) ?? "");
    if (isAccent(stored?.accent) && typeof stored.customColor === "string") {
      return [stored.accent, stored.customColor];
    }
  } catch {
    // ignore
  }
  return ["pink", DEFAULT_ACCENT_CUSTOM_COLOR];
};

/** Apply the persisted accent from AppSettings (the source of truth). */
export const syncAccentFromSettings = async (): Promise<void> => {
  try {
    const result = await commands.getAppSettings();
    if (result.status === "ok") {
      applyAccent(
        result.data.accent_color ?? "pink",
        result.data.accent_custom_color ?? DEFAULT_ACCENT_CUSTOM_COLOR,
      );
    }
  } catch (e) {
    console.warn("Failed to sync accent from settings:", e);
  }
};
