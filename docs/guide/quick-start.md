# Quick Start

There are three ways to get LCXL Remote Desk running, depending on your goal.

## Option 1: Docker Deployment (Recommended)

Start the service using Docker Compose:

```bash
printf 'LRD_BOOTSTRAP_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
docker-compose up -d
```

Access `http://localhost:8081`. In the first step, enter the deployment token
saved in `.env`, create the admin account, and accept the agreements. The wizard
then optionally connects a manager and finally configures inbound security and
telemetry. Keep `.env` after initialization because Compose validates the
required variable on every startup.

## Option 2: Tauri Desktop Client

Use this if you need locally-rendered enhancements like the **Privacy Screen** or **Whiteboard**:

```bash
cd tauri-app
cargo tauri dev
```

## Option 3: Run from Source (For Developers)

### Prerequisites

- Install the latest stable [Rust](https://www.rust-lang.org/) (Edition 2024, Rust 1.90+).
- Install [Node.js](https://nodejs.org/) 20 or higher.
- **AV1 Encoding (Optional)** — requires [nasm](https://www.nasm.us/) on Windows:

  ```bash
  $NASM_VERSION="2.15.05"
  $LINK="https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/win64"
  curl --ssl-no-revoke -LO "$LINK/nasm-$NASM_VERSION-win64.zip"
  7z e -y "nasm-$NASM_VERSION-win64.zip" -o "C:\nasm"
  set PATH="%PATH%;C:\nasm"
  ```

For platform-specific system dependencies (Linux / macOS), see [Deployment](/guide/deployment) and the project's `DEVELOPMENT.md`.

### Start the Backend

Signaling and Desktop services are enabled by default:

```bash
cargo run --release
```

### Start the Frontend

```bash
cd vite-project
npm ci
npm run dev
```

Access `http://localhost:5174`.

## Next Steps

- Learn how the pieces fit together in [Core Concepts](/guide/concepts).
- Understand the different process layouts in [Startup Modes](/guide/startup-modes).
- Tune behavior via the [config.toml Reference](/config/config-toml).
