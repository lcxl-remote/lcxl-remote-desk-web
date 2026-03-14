# Task Archive: Add Tray Notification for Security Approval

**Date**: 2026-03-14
**Goal**: Add a system tray notification (balloon message) when a security approval request is received in the Tauri application to alert the user.

## Implementation Plan

### 1. Dependencies and Configuration
- Add `tauri-plugin-notification` to `tauri-app/src-tauri/Cargo.toml`.
- Add `rust-i18n` for multi-language support.
- Register `tauri-plugin-notification` in `tauri::Builder` within `src/lib.rs`.
- Initialize `rust-i18n` with a local `locales` directory.

### 2. Localization
- Create `tauri-app/src-tauri/locales/v2.yml` using `rust-i18n` v2 format to store English and Chinese translations for security notifications.

### 3. Core Logic
- Update `SecurityApprovalManager::start` in `src/security_approval.rs` to:
    - Trigger a system notification when a request is received.
    - Map the `SecurityPermissionType` to localized strings using `rust-i18n::t!`.
    - Ensure the main window is focused and shown alongside the notification.

### 4. Permissions
- Update `tauri-app/src-tauri/capabilities/default.json` to include `"notification:default"`.

## Task List
- [x] Research Tauri v2 notification and tray API.
- [x] Add `tauri-plugin-notification` and `rust-i18n` dependencies.
- [x] Register plugin and initialize i18n in `lib.rs`.
- [x] Implement multi-language configuration in `locales/v2.yml`.
- [x] Update `SecurityApprovalManager` with notification logic.
- [x] Update app capabilities for notification permission.
- [x] Verify compilation with `cargo check`.

## Walkthrough
1. **Dependency Injection**: Added the official notification plugin to the Tauri backend.
2. **I18n Setup**: Configured `rust-i18n` to handle local translations in a single `v2.yml` file, aligning with the project's preferred patterns.
3. **Notification Integration**: Enhanced the security approval flow so that whenever a remote controller requests permission (e.g., Remote Control, File Transfer), a localized system notification pops up. This provides a clear visual cue even if the application window is hidden or minimized.
4. **Platform Compliance**: Added the necessary Tauri v2 permissions to the default capability manifest to ensure notifications are permitted by the OS.
5. **Validation**: Confirmed that the changes compile correctly and integrate seamlessly with the existing `SecurityApprovalManager`.

## Files Modified
- `tauri-app/src-tauri/Cargo.toml`
- `tauri-app/src-tauri/src/lib.rs`
- `tauri-app/src-tauri/src/security_approval.rs`
- `tauri-app/src-tauri/capabilities/default.json`
- `tauri-app/src-tauri/locales/v2.yml` (New)
