# Privacy Screen & Whiteboard

These two features render locally on the controlled machine and therefore require the **Tauri desktop client** (`tauri-app`).

## Privacy Screen

Lock the local display and input to ensure privacy during remote operations — bystanders at the remote machine cannot see the screen or interfere with input while you work.

Privacy-screen settings live under `[desk.private_screen]` in `config.toml`.

## Remote Whiteboard

Draw and annotate directly on the remote screen for collaboration — useful for guided support and demonstrations.

## Running the Tauri Client

```bash
cd tauri-app
cargo tauri dev
```

See [Quick Start → Tauri Desktop Client](/guide/quick-start#option-2-tauri-desktop-client).
