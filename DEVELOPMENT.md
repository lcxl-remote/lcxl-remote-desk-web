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
- **WebRTC**: webrtc-rs 0.13
- **Session Management**: Actix-Session with Cookie
- **Logging**: env_logger 0.11
- **Configuration**: config 0.15 (TOML)
- **API Documentation**: Utoipa 5 (Swagger, Redoc, RapiDoc, Scalar)
- **TURN Service**: turn-server 3.4
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

- **Video Capture**: Windows (DirectX), Linux (X11RB)
- **Video Encoding**: VP8, VP9 (libvpx)
- **Audio Capture**: Windows (WASAPI), Linux (ALSA, PipeWire)
- **Audio Encoding**: Opus (libopus)

### System Requirements

### Rust Development

- Rust 1.90 or higher
- Cargo

### Frontend Development

- Node.js 12.0.0 or higher
- npm, yarn, or pnpm

### Linux System Dependencies

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev clang libclang-dev cmake libvpx-dev
```

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
- `log_level`: Logging level (error, warn, info, debug, trace).
- `traceback`: Whether to enable Rust error backtrace.

#### User [user]

- `login_user_name`: Initial login username.
- `login_password`: Initial login password.

#### TURN Server [turn]

- `realm`: TURN server realm for authentication.
- `interfaces`: Network interface configuration. Supports `udp` and `tcp` protocols and ports.
- `static_credentials`: Static credentials including `user` and `password`.

#### Desktop [desk]

- `video_fps`: Video frame rate (default 60). Lowering this value reduces CPU and bandwidth usage.
- `video_encoder`: Video encoder. `VP8` or `VP9` are recommended.
- `audio_encoder`: Audio encoder. `OPUS` is primarily supported.
- `video_device_index`: Index of the monitor to capture (multi-monitor environments).
- `show_mouse`: Whether to capture and display the mouse cursor.

### Recommended Development Config

```toml
[system]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # Reduce FPS during development to save resources
```

## Development Workflow

### Project Structure

- `server/`: Main server application
- `signal/`: WebRTC signaling & TURN services
- `vite-project/`: React frontend
- `utils/`: Common utilities

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
- `-m, --startup-mode <MODE>`: Startup mode
  - `default`: Full mode with signaling and desk server
  - `signaling`: Signaling mode only (Signaling + TURN)
  - `desk-server`: Desk server mode only

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
