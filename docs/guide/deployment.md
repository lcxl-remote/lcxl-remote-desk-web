# Deployment

## Docker (Recommended)

```bash
docker-compose up -d
```

Access `http://localhost:8081` and set up the admin account on first visit. To build a custom image, use `./build_docker.sh`.

## Production Build from Source

```bash
# Backend
cargo build --release

# Frontend
cd vite-project
npm run build
```

## System Dependencies

### Linux

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev \
  libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev \
  clang libclang-dev cmake libvpx-dev
```

### macOS

Install via Homebrew (`x264` and `libvpx` are resolved through `pkg-config`; `cmake` builds the bundled Opus from source):

```bash
brew install pkgconf libvpx x264 cmake
```

On Apple Silicon, make sure `pkg-config` can locate the Homebrew `.pc` files:

```bash
export PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig:$PKG_CONFIG_PATH"
```

### Windows

No extra dependencies; everything is managed automatically through Cargo. (AV1 encoding optionally needs [nasm](https://www.nasm.us/) — see [Quick Start](/guide/quick-start).)

## Networking & NAT Traversal

The server bundles signaling, STUN, and TURN. For connections across NATs:

- Connections prefer **direct WebRTC P2P** and fall back to **TURN relay** only when traversal fails.
- For external access, ensure the **signaling** endpoint is reachable and that the configured **relay port range** is open/mapped.
- TURN realm, credentials, interfaces, and the relay port range are configured under `[turn]` — see the [config.toml Reference](/config/config-toml).

## Scaling the Process Layout

For multi-session hosts or capturing secure surfaces, run the [service-daemon mode](/guide/startup-modes), or split the signaling service out with `--startup-mode signaling`.

## Public Deployment Hardening

When exposing the server to the public internet (typically behind a TLS-terminating reverse proxy), note:

- **`LRD_COOKIE_SECURE`** — controls the session cookie `Secure` attribute. It defaults to `false` so a local / LAN HTTP setup keeps its session. Set `LRD_COOKIE_SECURE=true` for an HTTPS deployment so the cookie is only ever sent over HTTPS.
- **`LRD_PROVIDER_SSRF_MODE`** — guards the central brain's outbound dial to a user-configured model provider `base_url` against SSRF (an internal service or a cloud metadata endpoint). Governs **private reachability only** (orthogonal to the TLS switch below); the cloud-metadata ranges are blocked in every mode. Values:
  - `relaxed` (default) — allows private / loopback targets (local model gateways like `http://localhost:11434`).
  - `strict` — rejects private / loopback / CGNAT / ULA targets; re-validates the resolved IP at connect time (anti DNS-rebinding). Use this if untrusted users can configure the provider.
- **`LRD_ENFORCE_PUBLIC_TLS`** — whether a *public* target may be dialed over *plaintext* (`http`). Defaults to `true` (only an explicit `false` / `0` / `no` / `off` turns it off). When on, a plaintext dial to a public address is refused before connecting, so the api_key never leaves in the clear; private / loopback / LAN targets are always exempt and the cloud-metadata floor is always blocked regardless. Orthogonal to the SSRF mode: to allow a public plaintext provider, turn this off — you do **not** need `relaxed` (which would additionally open private targets).
- **Runtime API docs endpoints are not served** (Swagger UI / ReDoc / RapiDoc / Scalar / `/openapi.json`); generate the spec offline with `dump-openapi` (see the [REST API reference](/reference/api)).
- Put the server behind a reverse proxy that terminates TLS, passes through `Host`, and forwards the WebSocket `Upgrade` headers for signaling.
