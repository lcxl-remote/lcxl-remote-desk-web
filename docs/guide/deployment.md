# Deployment

## Docker (Recommended)

```bash
printf 'LRD_BOOTSTRAP_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
docker-compose up -d
```

Access `http://localhost:8081`, enter the token from `.env`, and set up the admin
account. Keep `.env` after initialization: Compose validates the required
variable on every startup. To build a custom image, use `./build_docker.sh`.

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

For unattended access, open **System Settings → macOS Status → Automatic
Login** in the web console. The card reports FileVault, the configured
automatic-login user and the current user, and provides copyable enable/disable
commands. The enable command uses `sysadminctl ... -password -`: run it in
Terminal so macOS requests the password interactively; the web page never reads
or handles the password. FileVault and automatic login are mutually exclusive,
so the page disables the enable action while FileVault is active.

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
- **`LRD_BOOTSTRAP_TOKEN`** — optional outside the provided Compose example; when set, the initialization wizard and its pre-initialization connection probes require this value. A present but blank value is a startup error. Use at least 32 random bytes and never put it in a URL or log. The Compose example makes it mandatory with `${...:?}`.
- **`LRD_TRUSTED_PROXIES`** — comma-separated proxy IP/CIDR list. Loopback (`127.0.0.0/8`, `::1`) is trusted by default; other proxy/container networks must be explicit. Only trusted peers may supply `X-Forwarded-For`. Do not use `*` unless the server cannot be reached except through a proxy that overwrites XFF.
- **`LRD_AUTH_IPV6_PREFIX_LEN`** — IPv6 rate-limit prefix, default `64` (`1..=128`). IPv4 always uses `/32`.
- **`LRD_AUTH_RATE_LIMIT_MAX_BUCKETS`** — bounded login and redeem capacity tier, default `65536`. Ordinary deployments should leave it unchanged.
- **`LRD_PROVIDER_SSRF_MODE`** — guards the central brain's outbound dial to a user-configured model provider `base_url` against SSRF (an internal service or a cloud metadata endpoint). Governs **private reachability only** (orthogonal to the TLS switch below); the cloud-metadata ranges are blocked in every mode. Values:
  - `relaxed` (default) — allows private / loopback targets (local model gateways like `http://localhost:11434`).
  - `strict` — rejects private / loopback / CGNAT / ULA targets; re-validates the resolved IP at connect time (anti DNS-rebinding). Use this if untrusted users can configure the provider.
- **`LRD_ENFORCE_PUBLIC_TLS`** — whether a *public* target may be dialed over *plaintext* (`http`). Defaults to `true` (only an explicit `false` / `0` / `no` / `off` turns it off). When on, a plaintext dial to a public address is refused before connecting, so the api_key never leaves in the clear; private / loopback / LAN targets are always exempt and the cloud-metadata floor is always blocked regardless. Orthogonal to the SSRF mode: to allow a public plaintext provider, turn this off — you do **not** need `relaxed` (which would additionally open private targets).
- **Runtime API docs endpoints are not served** (Swagger UI / ReDoc / RapiDoc / Scalar / `/openapi.json`); generate the spec offline with `dump-openapi` (see the [REST API reference](/reference/api)).
- Put the server behind a reverse proxy that terminates TLS, passes through `Host`, and forwards the WebSocket `Upgrade` headers for signaling.
- A native reverse proxy on the same host normally connects from loopback and works with the default trust. A proxy container normally connects from a bridge/container address, so add that actual peer CIDR explicitly. If Docker or a layer-4 proxy has already discarded the original source address and supplies no XFF, the application cannot reconstruct it and clients share one rate-limit bucket.
- The server currently has no CORS middleware. Browsers therefore cannot use a cross-origin preflight to send a custom XFF header to a loopback peer. Adding CORS, Private Network Access allowances, or XFF header allowances requires re-reviewing the default loopback trust boundary.
