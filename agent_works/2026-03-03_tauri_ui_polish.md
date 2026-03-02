# Tauri UI Polish & Private Screen Migration

**Date**: 2026-03-03
**Goal**: Enhance the Tauri application's user interface by addressing the initial white screen duration, implementing system tray functionality, integrating backend token auto-login, and migrating the private screen interface to the Vite SPA.

---

## 1. Initial White Screen Mitigation
* **Problem**: The Tauri application displayed a prolonged white screen during startup while waiting for the webview content (Vite app) to load.
* **Solution**: Handled in `tauri-app/src-tauri/src/lib.rs`. The `WebviewWindow` was initially configured with `.visible(false)`. A listener was added to `.on_page_load` catching `tauri::webview::PageLoadEvent::Finished`, which then subsequently triggers `window.show()` and `window.set_focus()`. 

## 2. System Tray and Background Operation
* **Problem**: Upon closing the main Tauri window, there was no way to reopen it without restarting the process.
* **Solution**: 
  - Utilizing `TrayIconBuilder` in `tauri-app` to establish a system tray icon.
  - Adding a `"LCXL Remote Desktop"` tooltip.
  - Inserting "Open Window" and "Exit" context menus.
  - Intercepting `tauri::WindowEvent::CloseRequested` to `hide()` the window instead of forcefully quitting the application.
  - Tracking true exit intentions via an `AtomicBool` (`IS_EXITING`). When users invoke "Exit" from the tray, `ExitRequested` is not prevented, allowing the app to terminate gracefully.

## 3. Tauri Auto-Login Integration
* **Problem**: Booting the React application inside Tauri requires bypassing the standard username/password flow if the backend server is appropriately initialized.
* **Solution**:
  - Implemented `TauriLoginToken` using `uuid::Uuid::new_v4()` during Tauri startup.
  - Created a new backend endpoint `/api/login/tauri` in `server/src/controller/login.rs` that accepts the one-time token and auto-logs in the user as `admin` when valid.
  - The single-use token enforces constant-time comparison to thwart timing attacks.
  - Modified Vite's `LoginPage` to intercept the `?token=xxx` URL parameter, verify the token via API call, fetch details, and navigate the user accordingly.

## 4. Migrating Private Screen to Vite
* **Problem**: The private screen page was a standalone vanilla HTML (`private-screen.html`) residing in `tauri-app/src`. It lacked styling parity with the rest of the application.
* **Solution**:
  - Engineered a new Shadcn + Tailwind UI React component at `vite-project/src/features/desk/private-screen-page.tsx`.
  - Added the route `/private-screen` onto strictly non-nested routing within `vite-project/src/app/router.tsx`.
  - Upgraded `PrivateScreenManager` in `tauri-app/src-tauri/src/private_screen.rs` to receive `frontend_url`. It now opens `WebviewUrl::External(format!("{}/private-screen", self.frontend_url).parse().unwrap())`.
  - Deleted the legacy HTML asset and removed the obsolete `"frontendDist"` from `tauri.conf.json`.

---

## Walkthrough & Validation
1. Startup performance validates that the window remains invisible until the frontend is thoroughly parsed and active, avoiding sudden flashes.
2. The user can smoothly close the window via 'X' without killing the parent process. The system tray continues to function, allowing re-emergence of the window and programmatic quitting.
3. Supplying `?token=` parameter securely signs the user directly into the desktop dashboard or system settings depending on `startup_mode`.
4. Triggering the **Private Screen** accurately spawns the newly styled Vite-based interface on top, fully localized, completely locking UI interaction, and still dismissible gracefully via `Ctrl + Alt + L`.
