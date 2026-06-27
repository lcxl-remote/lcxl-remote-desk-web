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

## 公网部署加固

把 server 暴露到公网时（通常置于终结 TLS 的反向代理之后），注意：

- **`LRD_COOKIE_SECURE`**——控制会话 Cookie 的 `Secure` 属性。默认 `false`，以便本地 / 局域网 HTTP 访问保留会话。HTTPS 部署应设 `LRD_COOKIE_SECURE=true`，使 Cookie 仅经 HTTPS 发送。
- **`LRD_PROVIDER_SSRF_MODE`**——防护中心大脑代用户拨号其配置的模型供应商 `base_url` 时的 SSRF（指向内网服务或云元数据端点）。取值：
  - `relaxed`（默认）——允许 `http` 与私网 / 回环目标（本地模型网关，如 `http://localhost:11434`），但仍拒绝云元数据段。
  - `strict`——仅 `https`；拒绝私网 / 回环 / CGNAT / ULA 与云元数据段；连接期再校验解析到的 IP（防 DNS 重绑定）。当不可信用户可配置供应商时使用。
  - `off`——不校验（仅限特殊自建场景）。
- **运行时不再提供 API 文档端点**（Swagger UI / ReDoc / RapiDoc / Scalar / `/openapi.json`）；用离线 `dump-openapi` 生成规范（见 [REST API 参考](/zh/reference/api)）。
- 把 server 置于反向代理之后，由其终结 TLS、透传 `Host`，并为信令转发 WebSocket `Upgrade` 头。
