# 部署

## Docker（推荐）

```bash
docker-compose up -d
```

访问 `http://localhost:8081`，首次访问时设置管理员账户。如需自定义镜像，使用 `./build_docker.sh`。

## 从源码构建生产版本

```bash
# 后端
cargo build --release

# 前端
cd vite-project
npm run build
```

## 系统依赖

### Linux

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev \
  libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev \
  clang libclang-dev cmake libvpx-dev
```

### macOS

通过 Homebrew 安装（`x264` 与 `libvpx` 经 `pkg-config` 解析；`cmake` 用于从源码构建内置 Opus）：

```bash
brew install pkgconf libvpx x264 cmake
```

在 Apple Silicon 上，确保 `pkg-config` 能找到 Homebrew 的 `.pc` 文件：

```bash
export PKG_CONFIG_PATH="/opt/homebrew/lib/pkgconfig:$PKG_CONFIG_PATH"
```

### Windows

无需额外依赖；一切由 Cargo 自动管理。（AV1 编码可选地需要 [nasm](https://www.nasm.us/)——见[快速开始](/zh/guide/quick-start)。）

## 网络与 NAT 穿透

server 内置信令、STUN 与 TURN。跨 NAT 连接时：

- 连接优先**直连 WebRTC P2P**，仅当穿透失败时才回退到 **TURN 中继**。
- 对外访问时，确保**信令**端点可达，且配置的**中继端口范围**已开放/映射。
- TURN realm、凭据、接口与中继端口范围在 `[turn]` 下配置——见 [config.toml 参考](/zh/config/config-toml)。

## 扩展进程布局

对于多会话主机或采集安全界面，运行 [service-daemon 模式](/zh/guide/startup-modes)，或用 `--startup-mode signaling` 把信令服务单独拆出。
