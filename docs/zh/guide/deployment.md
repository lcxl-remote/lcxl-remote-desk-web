# 部署

## Docker（信令服务器推荐方式）

```bash
printf 'LRD_BOOTSTRAP_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
docker compose up -d
```

访问 `http://localhost:8081`，填写 `.env` 中的令牌并设置管理员账户。初始化后仍需保留
`.env`，Compose 每次启动都会校验该必填变量。如需自定义镜像，使用 `./build_docker.sh`。

### 需要持久化的状态

服务端写入的是 [config.toml 参考](/zh/config/config-toml)中说明的平台标准路径，而不是运行
目录。容器内以 root 运行，因而落在 Linux 系统级路径上，所以 Compose 文件把三个必须比容器
活得更久的目录做了 bind mount：

| 宿主机路径 | 容器内路径 | 内容 |
|---|---|---|
| `./conf` | `/etc/lcxl-remote-desk` | `config.toml` |
| `./data` | `/var/lib/lcxl-remote-desk` | 信令与执行台账数据库、远程访问状态 |
| `./logs` | `/var/log/lcxl-remote-desk` | 滚动日志 |

丢失 `./data` 会一并丢掉信令侧状态、访问码、AI 服务商配置与审计记录，请与 `./conf` 一起备份。

### TURN 中继端口

中继端口范围（`[turn]` 的 `relay_min_port` / `relay_max_port`，默认 `50000-50050`）**默认
不映射**。若要由这台服务器承担中继，请在 `docker-compose.yml` 中取消该端口范围的注释、在安全组
放行，并把 `[[turn.interfaces]]` 的 `external` 设为本机公网地址——见下方[网络与 NAT 穿透](#网络与-nat-穿透)。

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

如需无人值守访问，请在 Web 控制台打开**系统设置 → macOS 状态 → 自动登录**。
卡片会显示 FileVault、已配置的自动登录用户与当前用户，并提供可复制的启用/禁用命令。
启用命令使用 `sysadminctl ... -password -`：请在终端中执行，由 macOS 交互式请求密码；
网页绝不读取或处理密码。FileVault 与自动登录互斥，因此 FileVault 开启时页面会禁用
启用操作。

### Windows

无需额外依赖；一切由 Cargo 自动管理。（AV1 编码可选地需要 [nasm](https://www.nasm.us/)——见[快速开始](/zh/guide/quick-start)。）

## 网络与 NAT 穿透

server 内置信令、STUN 与 TURN。跨 NAT 连接时：

- 连接优先**直连 WebRTC P2P**，仅当穿透失败时才回退到 **TURN 中继**。
- 对外访问时，确保**信令**端点可达，且配置的**中继端口范围**已开放/映射。
- TURN realm、凭据、接口与中继端口范围在 `[turn]` 下配置——见 [config.toml 参考](/zh/config/config-toml)。

## 扩展进程布局

对于多会话主机或采集安全界面，运行 [service-daemon 模式](/zh/guide/startup-modes)，或用 `--startup-mode signaling` 把信令服务单独拆出。

## 本地 Computer Use 应用策略

被控端的 **系统设置 → AI 策略 → 应用限制 → 高级** 提供可选的完整可执行路径限制。读取和保存同时要求 owner 登录态、内核报告的 loopback 对端以及匹配的 loopback Origin/Host。请通过 `localhost` 或 `127.0.0.1` 打开本地页面；远端页面和 Manager 不代理此策略。

`computer_use.allowed_application_paths = []` 表示**不额外限制应用**，不再表示拒绝所有应用；已有非空列表继续生效。macOS 应填写可执行文件路径（例如 `/System/Applications/Calculator.app/Contents/MacOS/Calculator`），不是 `.app` 目录。该策略作用于通用 UI 观察、语义 UI 操作及裸输入回退，不授予模型外发或动作权限，不替代 TCC、精确目标绑定和助手总开关。

保存检查当前 revision，经设置协调器持久化，并等待全部存活 worker（含便携模式）精确确认。确认失败会返回错误并停止旧 worker，请重启被控端后重新读取策略；磁盘失败则保留原有生效值。不要在运行中直接改配置文件绕过版本检查。

同机验证可选择 **5 秒后观察**，然后切换目标应用。请求观察的是执行时的被控端前台应用，不提供后台应用读取；取消、切换设备、关闭助手和离开页面都会撤销倒计时且不采集内容。

## 公网部署加固

把 server 暴露到公网时（通常置于终结 TLS 的反向代理之后），注意：

- **`LRD_COOKIE_SECURE`**——控制会话 Cookie 的 `Secure` 属性。默认 `false`，以便本地 / 局域网 HTTP 访问保留会话。HTTPS 部署应设 `LRD_COOKIE_SECURE=true`，使 Cookie 仅经 HTTPS 发送。
- **`LRD_BOOTSTRAP_TOKEN`**——在 Compose 示例以外是可选项；配置后，初始化向导及初始化前的连接探测都必须携带该值。变量存在但为空会导致启动失败。请使用至少 32 个随机字节，不要放入 URL 或日志。Compose 示例通过 `${...:?}` 强制要求它。
- **`LRD_TRUSTED_PROXIES`**——逗号分隔的代理 IP/CIDR。默认信任回环地址（`127.0.0.0/8`、`::1`），其他代理或容器网段必须显式配置。只有可信 peer 才能提供 `X-Forwarded-For`。除非服务器只能经会覆盖 XFF 的代理访问，否则不要使用 `*`。
- **`LRD_AUTH_IPV6_PREFIX_LEN`**——IPv6 限流前缀，默认 `64`，合法范围 `1..=128`；IPv4 固定为 `/32`。
- **`LRD_AUTH_RATE_LIMIT_MAX_BUCKETS`**——登录和 redeem 的有界容量档位，默认 `65536`，普通部署无需调整。
- **`LRD_PROVIDER_SSRF_MODE`**——防护中心大脑代用户拨号其配置的模型供应商 `base_url` 时的 SSRF（指向内网服务或云元数据端点）。**只管私网可达性**（与下方 TLS 开关正交）；云元数据段在任何模式下都被拦截。取值：
  - `relaxed`（默认）——允许私网 / 回环目标（本地模型网关，如 `http://localhost:11434`）。
  - `strict`——拒绝私网 / 回环 / CGNAT / ULA 目标；连接期再校验解析到的 IP（防 DNS 重绑定）。当不可信用户可配置供应商时使用。
- **`LRD_ENFORCE_PUBLIC_TLS`**——是否允许以**明文**（`http`）拨号**公网**目标。默认 `true`（仅显式设为 `false` / `0` / `no` / `off` 才关闭）。开启时，对公网地址的明文拨号会在连接前被拒（api_key 绝不明文外泄）；私网 / 回环 / 局域网目标始终豁免，云元数据段无论如何始终拦截。与 SSRF 模式正交：要放行公网明文供应商，关闭本开关即可，**无需**切到 `relaxed`（那会额外放开私网目标）。
- **Web Search**——在中央服务器的**系统设置 → 网页搜索**（`/system/web-search`）配置，内嵌及独立信令模式均支持。首次初始化默认 DuckDuckGo，无需 API Key；也可选择 Brave 或 Tavily 并填写对应密钥。配置持久化于 signal 的 SQLite 数据库，不再读取 `LRD_BRAVE_SEARCH_API_KEY`；已有开发环境密钥需在新页面重新填写。密钥只写不回显，切换厂商清除前一家密钥；选择 API 厂商后缺少密钥时不可用，不会自动回退。
- **测试与授权**——加载和保存配置不触发搜索。**测试连接**仅发送固定公开词“Rust programming language”，可能消耗厂商配额，不保存当前编辑、不使用会话内容；它是独立于设备 AI 助手任务的管理连接测试。助手查询仍须逐字来自当前用户消息，通过精确 ExportData 授权并遵循助手总开关。切换厂商或更新配置后，旧搜索授权不能用于后续派发；配置冲突或保存结果不确定时请重新加载。DuckDuckGo 免 API 配置不代表无限请求，限流、验证码及异常页面正常报错，不伪装为空结果；模型费用另计。
- **没有运行时 API 文档端点**：server 不提供 Swagger UI / ReDoc / RapiDoc / Scalar，也不提供 `/openapi.json`，公网上因此没有这一类可被探测的面。需要规范时用离线 `dump-openapi` 生成（见 [REST API 参考](/zh/reference/api)）。
- 把 server 置于反向代理之后，由其终结 TLS、透传 `Host`，并为信令转发 WebSocket `Upgrade` 头。
- 同机原生反向代理通常从 loopback 连接，可直接使用默认信任；代理容器通常从 bridge/container 地址连接，必须显式加入其实际 peer CIDR。若 Docker 或四层代理已经丢失真实源地址且不提供 XFF，应用无法恢复，所有客户端只能共享一个限流 bucket。
- server 当前没有 CORS 中间件，浏览器无法通过跨源预检向 loopback peer 发送自定义 XFF。未来增加 CORS、Private Network Access 放行或 XFF 请求头放行时，必须重新评审默认 loopback 信任边界。
