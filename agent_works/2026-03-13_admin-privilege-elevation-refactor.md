# Archival - Administrative Privilege Check and Elevation Refactor (2026-03-13)

## Objective
The goal was to implement a robust administrative privilege check for the remote desktop server, improve the UX for privilege elevation (automatic exit), and consolidate security warnings in the frontend with proper internationalization.

## Completed Tasks

### Infrastructure & Signaling
- [x] Implemented `is_admin()` in `desk-utils` using native APIs (Windows: `IsUserAnAdmin`, Unix: `getuid() == 0`).
- [x] Updated `SystemInfo` and `ServerInfo` APIs to report administrative status.
- [x] Integrated `is_admin` into `InitSignalingData` (signaling protocol) to notify the controller of the host's privilege level.
- [x] Refactored `StartupMode` to be a strictly typed Enum (`StartupMode`) instead of a raw string, improving API consistency and type safety.

### Tauri Desktop App
- [x] Modified the elevation logic to call `std::process::exit(0)` after successfully launching the elevated process. This ensures the non-privileged instance closes immediately after elevation.

### Web Frontend
- [x] Consolidated administrative privilege warnings: removed them from general settings/initialization pages and moved them to the `DeskConfigDialog`.
- [x] Implemented internationalization (i18n) for the new warning messages in both English and Chinese.
- [x] Synchronized OpenAPI client types with the updated backend models, handling the transition from `Option<bool>` to `bool` for signaling data.

## Implementation Details

### Backend
The `is_admin` check is now performed during signaling initialization and reported via the `SystemInfo` endpoint. The `StartupMode` was refactored to a `ToSchema` enabled Enum for better OpenAPI documentation.

### Frontend
The `DeskConfigDialog` now conditionally renders a styled `Alert` when `initData.is_admin` is false, ensuring users are aware of potential limitations (like keyboard/mouse control or audio capture) specifically when they are about to start a session.

## Verification
- **Builds**: Both backend (`cargo build`) and frontend (`npm run build`) completed successfully with zero type errors.
- **Manual Test**: Verified that elevation triggers a clean exit of the original process and that the warning UI adapts to language changes.
