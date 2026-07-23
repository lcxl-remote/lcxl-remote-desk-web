# Desktop Language Synchronization

The language selector behaves differently in the Tauri desktop shell and in a
normal browser.

## Desktop Shell

In the desktop app, changing language updates one host-wide locale. The choice
is persisted in `[system].locale` and immediately refreshes:

- the current web interface and other open desktop windows;
- tray menu text, tray tooltip, and native window titles;
- native dialogs that are opened after the change;
- the daemon and the active session worker.

The persisted locale is applied before the tray and first native window are
created, so startup UI does not briefly use another language.

The desktop shell waits for the native bridge to confirm persistence before it
changes the web interface. If the bridge is not ready or the write fails, the
page reports an error and does not silently switch only the web content.

## Browser

Changing language in a normal browser remains a browser-local preference. It
updates that browser's `localStorage` and i18next instance but does not modify
the host daemon, workers, tray, or native dialogs.

Supported locale tags are `en-US` and `zh-CN`.

## Security and Multiple Windows

Each Tauri WebSocket session receives its own short-lived native bridge token.
The token is sent only to that session and is revoked on disconnect. Locale
change notifications are broadcast separately so every currently open desktop
window converges to the same committed language.

