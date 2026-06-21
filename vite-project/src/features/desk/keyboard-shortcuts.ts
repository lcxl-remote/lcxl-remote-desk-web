import type { OperationSystemEnum } from "@/services/types";
import type { SyntheticKeyEvent } from "./use-desk-input";

// Windows virtual-key codes used to build the host-targeted shortcut chords.
// The host maps these to its own native keys when injecting — notably on macOS
// VK_LWIN (91) → Command, VK_CONTROL (17) → Control, VK_MENU (18) → Option.
const VK = {
    SHIFT: 16,
    CTRL: 17,
    ALT: 18,
    META: 91,
    ESC: 27,
    TAB: 9,
    SPACE: 32,
    F4: 115,
    UP: 38,
    DIGIT4: 52,
    D: 68,
    E: 69,
    L: 76,
    Q: 81,
    R: 82,
    W: 87,
    DEL: 46,
} as const;

/**
 * Press the given keys in order, then release them in reverse — the standard
 * way to emit a modifier chord (e.g. Ctrl down, Alt down, Del down, Del up,
 * Alt up, Ctrl up). Releasing in reverse keeps modifiers held until the
 * non-modifier key has been released.
 */
function chord(...keyCodes: number[]): SyntheticKeyEvent[] {
    const down = keyCodes.map(keyCode => ({ event: "keydown" as const, keyCode }));
    const up = [...keyCodes].reverse().map(keyCode => ({ event: "keyup" as const, keyCode }));
    return [...down, ...up];
}

export type KeyboardShortcut = {
    /** Stable key for React lists and tests. */
    id: string;
    /** i18n key for the menu label. */
    labelKey: string;
    /** Fallback label if the i18n key is missing. */
    labelFallback: string;
    /** Ordered synthetic key events sent to the host. */
    events: SyntheticKeyEvent[];
};

const WINDOWS_SHORTCUTS: KeyboardShortcut[] = [
    { id: "ctrlAltDel", labelKey: "pages.desk.shortcut.ctrlAltDel", labelFallback: "Ctrl + Alt + Del", events: chord(VK.CTRL, VK.ALT, VK.DEL) },
    { id: "taskManager", labelKey: "pages.desk.shortcut.taskManager", labelFallback: "Ctrl + Shift + Esc (Task Manager)", events: chord(VK.CTRL, VK.SHIFT, VK.ESC) },
    { id: "altF4", labelKey: "pages.desk.shortcut.altF4", labelFallback: "Alt + F4 (Close Window)", events: chord(VK.ALT, VK.F4) },
    { id: "altTab", labelKey: "pages.desk.shortcut.altTab", labelFallback: "Alt + Tab (Switch Window)", events: chord(VK.ALT, VK.TAB) },
    { id: "winKey", labelKey: "pages.desk.shortcut.winKey", labelFallback: "Windows Key", events: chord(VK.META) },
    { id: "winD", labelKey: "pages.desk.shortcut.winD", labelFallback: "Win + D (Show Desktop)", events: chord(VK.META, VK.D) },
    { id: "winE", labelKey: "pages.desk.shortcut.winE", labelFallback: "Win + E (File Explorer)", events: chord(VK.META, VK.E) },
    { id: "winR", labelKey: "pages.desk.shortcut.winR", labelFallback: "Win + R (Run)", events: chord(VK.META, VK.R) },
    { id: "winL", labelKey: "pages.desk.shortcut.winL", labelFallback: "Win + L (Lock)", events: chord(VK.META, VK.L) },
];

const MACOS_SHORTCUTS: KeyboardShortcut[] = [
    { id: "forceQuit", labelKey: "pages.desk.shortcut.forceQuit", labelFallback: "⌘ + ⌥ + Esc (Force Quit)", events: chord(VK.META, VK.ALT, VK.ESC) },
    { id: "lockScreen", labelKey: "pages.desk.shortcut.lockScreen", labelFallback: "⌘ + ⌃ + Q (Lock Screen)", events: chord(VK.META, VK.CTRL, VK.Q) },
    { id: "spotlight", labelKey: "pages.desk.shortcut.spotlight", labelFallback: "⌘ + Space (Spotlight)", events: chord(VK.META, VK.SPACE) },
    { id: "switchApp", labelKey: "pages.desk.shortcut.switchApp", labelFallback: "⌘ + Tab (Switch App)", events: chord(VK.META, VK.TAB) },
    { id: "missionControl", labelKey: "pages.desk.shortcut.missionControl", labelFallback: "⌃ + ↑ (Mission Control)", events: chord(VK.CTRL, VK.UP) },
    { id: "screenshot", labelKey: "pages.desk.shortcut.screenshot", labelFallback: "⌘ + ⇧ + 4 (Screenshot)", events: chord(VK.META, VK.SHIFT, VK.DIGIT4) },
    { id: "closeWindow", labelKey: "pages.desk.shortcut.closeWindow", labelFallback: "⌘ + W (Close Window)", events: chord(VK.META, VK.W) },
    { id: "quitApp", labelKey: "pages.desk.shortcut.quitApp", labelFallback: "⌘ + Q (Quit App)", events: chord(VK.META, VK.Q) },
];

/**
 * Host-targeted keyboard-shortcut menu for the given remote OS. macOS hosts get
 * the Command-based set; every other (or unknown) host falls back to the
 * Windows set, which is also a reasonable default for Linux desktops.
 */
export function getKeyboardShortcuts(os: OperationSystemEnum | undefined): KeyboardShortcut[] {
    return os === "Mac" ? MACOS_SHORTCUTS : WINDOWS_SHORTCUTS;
}
