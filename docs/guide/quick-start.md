# Quick Start

There are three ways to get LCXL Remote Desk running. Pick the one that matches your network.

## Option 1: Download and Run the Host (Recommended)

**Best when the controlled device is on your LAN or has a public IP of its own.** The host bundles signaling, STUN / TURN, and the web console, so no extra server is involved and the browser connects to it directly.

1. Download the host package for your platform from the [Releases page](https://github.com/lcxl/lcxl-remote-desk-web/releases):

   | Platform | Package |
   |---|---|
   | Windows x86_64 | `tauri-windows-x86_64.zip` |
   | Linux x86_64 | `tauri-linux-x86_64.zip` |
   | macOS Apple Silicon | `tauri-macos-aarch64.dmg` |
   | macOS Intel | `tauri-macos-x86_64.dmg` |

   Every package is the Tauri desktop shell. It embeds the host server in-process — running in [`default` mode](/guide/startup-modes), with signaling, STUN / TURN and the web console — and adds the locally-rendered [Privacy Screen and Whiteboard](/features/privacy-whiteboard).

2. Run it:

   - **Windows / Linux** — unpack the zip and launch `lcxl-remote-desk-tauri`. The archive holds that executable plus `lcxl-remote-desk-server` and a `static/` directory (the web console assets); **keep all three side by side**.
   - **macOS** — open the `.dmg` and drag **LCXL Remote Desktop** into Applications, then launch it.

3. The shell opens the console in its own window and finishes setup there. From another machine on the network the same console is at `http://<host-address>:8081`. The wizard creates the admin account, optionally connects a manager, and configures inbound security and telemetry. After that, control the device from the same LAN — or from anywhere that can reach its public IP.

::: tip No public IP, but the device can reach the internet?
In the wizard's connection step — or the **Outbound Connection** settings page afterwards — set the manager domain to the public server `lcxbox.app` and paste an API token created in its console. It handles signaling and NAT traversal, and control ends then reach the device through `https://lcxbox.app`.

That public server currently runs in the United States, so **access from outside the US may be slow or fail outright**. If latency matters or the link is unreliable, self-host signaling with Option 2 instead.
:::

## Option 2: Self-Hosted Signaling Server

Use this when the controlled device has no public IP and you want to own the whole path: rent a VPS with a public IP, run signaling on it, and point the host at that address.

1. On the VPS, clone the repository and start the service. The image starts in `signaling` mode, hosting the web control plane, signaling, and optional TURN relay; capture and input injection stay on the controlled devices outside the container:

   ```bash
   git clone https://github.com/lcxl/lcxl-remote-desk-web.git
   cd lcxl-remote-desk-web
   printf 'LRD_BOOTSTRAP_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
   docker compose up -d
   ```

2. Open `http://<vps-address>:8081`. In the first step, enter the deployment token saved in `.env`, create the admin account, and accept the agreements. Keep `.env` after initialization because Compose validates the required variable on every startup.

3. Harden the endpoint before relying on it: TLS on a reverse proxy, TURN interfaces and relay ports, and the `LRD_*` variables are all covered in [Deployment](/guide/deployment).

4. Copy the token from the signaling server's **Signaling Access Token** page. Run the host as in Option 1, then on its **Outbound Connection** settings page set the signaling URL to `wss://<your-domain>/api/desk/signaling` and paste that token.

::: warning
Hosts refuse plaintext `ws://` dials to **public** signaling addresses by default (`require_secure_signaling` in the [config.toml reference](/config/config-toml)). Loopback, private, and LAN addresses are exempt, so a LAN-only deployment without TLS can use `ws://<vps-address>:8081/api/desk/signaling`.
:::

## Option 3: Run from Source (For Developers)

### Prerequisites

- Install the repository-pinned [Rust](https://www.rust-lang.org/) toolchain (Edition 2024, Rust 1.90).
- Install [Node.js](https://nodejs.org/) 22.16 or higher.
- **AV1 Encoding (Optional)** — requires [nasm](https://www.nasm.us/) on Windows:

  ```bash
  $NASM_VERSION="2.15.05"
  $LINK="https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/win64"
  curl --ssl-no-revoke -LO "$LINK/nasm-$NASM_VERSION-win64.zip"
  7z e -y "nasm-$NASM_VERSION-win64.zip" -o "C:\nasm"
  set PATH="%PATH%;C:\nasm"
  ```

For platform-specific system dependencies (Linux / macOS), see [Deployment](/guide/deployment) and the project's `DEVELOPMENT.md`.

### Start the Frontend First

A debug build of the desktop shell loads the Vite dev server, so it opens a blank window if the frontend is not up yet:

```bash
cd vite-project
npm ci
npm run dev
```

The dev server listens on `http://localhost:5174`.

### Then Start the Host Shell

```bash
cargo run -p lcxl-remote-desk-tauri
```

It embeds the full server and adds the Privacy Screen and Whiteboard. For a headless backend without the GUI shell, run `cargo run -p lcxl-remote-desk-server` instead and open `http://localhost:5174` in a browser.

## Next Steps

- Learn how the pieces fit together in [Core Concepts](/guide/concepts).
- Understand the different process layouts in [Startup Modes](/guide/startup-modes).
- Tune behavior via the [config.toml Reference](/config/config-toml).
