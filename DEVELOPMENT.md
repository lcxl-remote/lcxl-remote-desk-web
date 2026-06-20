# Development Guide

[简体中文](DEVELOPMENT_CN.md)

This document provides a comprehensive guide for the LCXL Remote Desk Web project, including environment setup, development workflow, API documentation, and coding standards.

## Table of Contents

- [Requirements](#requirements)
- [Quick Start](#quick-start)
- [Configuration Details](#configuration-details)
- [API Documentation](#api-documentation)
- [CLI Arguments](#cli-arguments)
- [Development Workflow](#development-workflow)
- [Coding Standards](#coding-standards)
- [Debugging Tips](#debugging-tips)
- [Building and Deployment](#building-and-deployment)
- [FAQ](#faq)

## Requirements

### Tech Stack

#### Backend

- **Language**: Rust (Edition 2024, Rust 1.90+)
- **Web Framework**: Actix-Web 4.11
- **WebRTC**: webrtc-rs 0.17
- **Session Management**: Actix-Session with Cookie
- **Logging**: env_logger 0.11
- **Configuration**: config 0.15 (TOML)
- **API Documentation**: Utoipa 5 (Swagger, Redoc, RapiDoc, Scalar)
- **TURN Service**: turn 0.17
- **Monitoring**: Prometheus 0.13.4

#### Frontend

- **Framework**: React 19
- **UI Components**: TailwindCSS + Shadcn UI (Radix UI)
- **Build Tool**: Vite 7
- **Code Generation**: Kubb (OpenAPI to React Query/TypeScript)
- **Language**: TypeScript 5.9
- **Terminal Emulator**: xterm.js 5.5
- **State Management**: TanStack Query (React Query) v5

#### Multimedia

- **Video Capture**: Windows (DXGI / WGC), Linux (X11 / Wayland portal + PipeWire)
- **Video Encoding**: X264 / OpenH264 (H.264), VP8 / VP9 (libvpx), AV1 (rav1e)
- **Audio Capture**: Windows (WASAPI), Linux (ALSA / PipeWire)
- **Audio Encoding**: Opus (libopus)

### System Requirements

### Rust Development

- Rust 1.90 or higher
- Cargo

### Frontend Development

- Node.js 20 or higher (required by Vite 7)
- npm (the frontend uses npm)

### Linux System Dependencies

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev clang libclang-dev cmake libvpx-dev
```

### macOS System Dependencies

Install via Homebrew (`x264` and `libvpx` are resolved through `pkg-config`; `cmake` is required to build the bundled Opus from source):

```bash
brew install pkgconf libvpx x264 cmake
```

On Apple Silicon, make sure `pkg-config` can locate the Homebrew `.pc` files:

```bash
export PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig:$PKG_CONFIG_PATH"
```

### Windows System Dependencies

No extra dependencies; everything is managed automatically through Cargo.

## Quick Start

### 1. Backend Development

Configure `conf/config.toml` and run:

```bash
cargo run
```

### 2. Frontend Development

```bash
cd vite-project
npm install
npm run dev
```

## Configuration Details

### Server Configuration (conf/config.toml)

#### System [system]

- `enable_ipv6`: Whether to enable IPv6 support.
- `port`: Server listening port.
- `listen_addr_ipv4`: IPv4 listening address.
- `listen_addr_ipv6`: IPv6 listening address.

#### Log [log]

- `log_level`: Logging level (error, warn, info, debug, trace).
- `traceback`: Whether to enable Rust error backtrace.
- `log_retention_days`: Log retention in days (default 7).
- `log_cleanup_threshold_percent`: Disk usage threshold that triggers cleanup (default 90).
- `log_cleanup_interval_hours`: Interval in hours for the cleanup task (default 12).
- `tokio_console_enabled`: Enable the tokio-console subscriber (requires the `tokio_unstable` build flag, default false).

#### User [user]

- `login_user_name`: Initial login username.
- `login_password`: Initial login password.

#### TURN Server [turn]

- `realm`: TURN server realm for authentication.
- `interfaces`: Network interface configuration (`udp` / `tcp` protocols, listen and external addresses).
- `static_auth_secret`: Static authentication secret.
- `enable_stun` / `enable_turn`: Toggle STUN and TURN relay respectively.
- `relay_min_port` / `relay_max_port`: Relay port allocation range.
- `[turn.static_credentials]`: Optional static username / password credential table.

#### Desktop [desk]

- `video_fps`: Video frame rate (default 60). Lowering this value reduces CPU and bandwidth usage.
- `video_quality`: Video encoding quality (0-63, lower is better, default 22).
- `video_encoder` / `audio_encoder`: Optional; auto-selected when omitted. Video may be `X264` / `VP8` / `VP9` / `H264` / `AV1`; audio is `OPUS`.
- `video_device_name`: GDI device name of the monitor to capture (`\\.\DISPLAYn`); empty string means "ask the browser to pick on first connection".
- `show_mouse`: Whether to capture and display the mouse cursor.
- `enable_dirty_rect`: Whether to enable dirty-rectangle incremental encoding.
- `[desk.private_screen]`: Privacy screen settings (`enabled`, etc.).

#### Virtual Display [virtual_display]

- `enabled`: Whether to enable the virtual display (requires an installed IddCx driver; effective only in specific modes).
- `exclusive` / `prompt_ms` / `adaptive_*`: Exclusive-mode and adaptive-resolution parameters.

### Recommended Development Config

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # Reduce FPS during development to save resources
```

## Development Workflow

### Project Structure

- `server/`: Main server application (supports default / signaling / desk-server / service-daemon / session-worker startup modes; `daemon/` and `worker/` hold the system daemon and session worker)
- `signal/`: WebRTC signaling & TURN services
- `signal-facade/`: Shared signaling protocol models
- `turn/`: TURN/STUN service
- `capture-engine/`: Screen / audio capture and encoding
- `input-injection/`: Mouse / keyboard input injection and clipboard
- `ipc-protocol/`: IPC message definitions for daemon ↔ worker
- `virtual-display/`: Virtual display (IddCx) userspace wrapper
- `vite-project/`: React frontend
- `tauri-app/`: GUI desktop edition (Privacy Screen / Whiteboard)
- `utils/` / `server-version/`: Common utilities and API version constants

## API Documentation

Once the server is running, access documentation at:

- **Swagger UI**: `http://localhost:8081/swagger-ui/`
- **ReDoc**: `http://localhost:8081/redoc`
- **RapiDoc**: `http://localhost:8081/rapidoc`
- **Scalar**: `http://localhost:8081/scalar`

API spec definition: `http://localhost:8081/openapi.json`

## CLI Arguments

```bash
cargo run -- --help
```

Available arguments:

- `-c, --config-file-path <PATH>`: Path to configuration file (default: conf/config)
- `-s, --startup-mode <MODE>`: Startup mode
  - `default`: Full mode with signaling and desk server
  - `signaling`: Signaling mode only (Signaling + TURN)
  - `desk-server`: Desk server mode only
  - `service-daemon`: System service daemon (SYSTEM / root) that manages session workers
  - `session-worker`: Worker process launched by the daemon inside the user's desktop session

### Adding Features

1. Define models in `server/src/model/`.
2. Implement logic in `server/src/service/`.
3. Add route handlers in `server/src/controller/`.
4. Register routes in `server/src/main.rs`.

## Coding Standards

- **Rust**: Follow `rustfmt` and run `cargo clippy`.
- **Frontend**: Follow ESLint and Prettier.

## Debugging Tips

### Backend Debugging

1. **Log Level**: Set `log_level = "debug"` or `"trace"` in `config.toml`.
2. **Environment Variables**: Use `RUST_LOG=debug cargo run` to override log level.
3. **Error Backtrace**: Set `traceback = true` in `config.toml`.

### Frontend Debugging

1. **Dev Server**: Run `npm run dev` for hot reloading.
2. **Browser DevTools**: Use Chrome/Firefox DevTools for debugging.
3. **React DevTools**: Use the React browser extension for component inspection.

## Building and Deployment

### Production Build

```bash
# Backend
cargo build --release

# Frontend
cd vite-project
npm run build
```

### Docker

Use `./build_docker.sh` for easy building, or `docker-compose` for quick deployment.

## FAQ

### Compilation Errors

**Q: Missing system dependency libraries.**
A: Ensure all required system dependencies are installed (see [Requirements](#requirements)).

**Q: Rust version is too old.**
A: Run `rustup update` to update Rust to the latest version.

### Runtime Errors

**Q: Port already in use.**
A: Modify the `port` setting in `config.toml`.

**Q: WebRTC connection failed.**
A: Check STUN/TURN server configuration and ensure network connectivity. For external access, verify the signaling server mode is correctly started and ports are mapped.
