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
