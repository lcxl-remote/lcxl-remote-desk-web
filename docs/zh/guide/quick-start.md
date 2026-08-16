# 快速开始

运行 LCXL Remote Desk 有三种方式，按你的网络环境选择。

## 方式一：下载被控端直接运行（推荐）

**被控机在局域网内、或本身有公网 IP 时最合适。**被控端自带信令、STUN / TURN 与 Web 控制台，不需要任何额外服务器，浏览器直连即可。

1. 从 [Releases 页面](https://github.com/lcxl/lcxl-remote-desk-web/releases)下载对应平台的被控端安装包：

   | 平台 | 安装包 |
   |---|---|
   | Windows x86_64 | `tauri-windows-x86_64.zip` |
   | Linux x86_64 | `tauri-linux-x86_64.zip` |
   | macOS Apple Silicon | `tauri-macos-aarch64.dmg` |
   | macOS Intel | `tauri-macos-x86_64.dmg` |

   这些包都是 Tauri 桌面外壳：它在进程内内嵌被控端服务（以 [`default` 模式](/zh/guide/startup-modes)运行，自带信令、STUN / TURN 与 Web 控制台），并额外提供本地渲染的[防窥屏与白板](/zh/features/privacy-whiteboard)。

2. 运行：

   - **Windows / Linux**：解压后运行 `lcxl-remote-desk-tauri`。压缩包内还有 `lcxl-remote-desk-server` 与 `static/`（Web 控制台静态资源），**三者必须保持同级**。
   - **macOS**：打开 `.dmg`，把 **LCXL Remote Desktop** 拖入「应用程序」后启动。

3. 外壳会自带窗口打开控制台，初始化在其中完成；局域网内其他机器访问同一控制台的地址是 `http://<被控机地址>:8081`。向导会引导你创建管理员账户、可选地连接 Manager，并设置入站安全策略与遥测选项。之后即可从同一局域网（或任何能直连该公网 IP 的网络）远程控制这台设备。

::: tip 没有公网 IP，但被控机能访问公网？
可以在向导的连接步骤（或之后的**出站连接**设置页）把 Manager 域名填成公共服务器 `lcxbox.app`，并粘贴在其控制台创建的 API 令牌，由它完成信令与 NAT 穿透，控制端随后从 `https://lcxbox.app` 访问该设备。

该公共服务器目前部署在美国，**非美国地区访问可能较慢，甚至无法连通**；对时延敏感或访问不畅时，请改用下面的方式二自建信令服务器。
:::

## 方式二：自建信令服务器

被控机没有公网 IP，又希望链路完全自主可控时使用：向云服务商购买一台有公网 IP 的 VPS，在上面跑信令服务，被控端把信令地址指向它。

1. 在 VPS 上克隆仓库并启动服务。镜像默认以 `signaling` 模式启动，承载 Web 控制台、信令与可选 TURN 中继；桌面采集和输入注入仍由容器外的被控设备完成：

   ```bash
   git clone https://github.com/lcxl/lcxl-remote-desk-web.git
   cd lcxl-remote-desk-web
   printf 'LRD_BOOTSTRAP_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
   docker compose up -d
   ```

2. 访问 `http://<VPS 地址>:8081`。第一步填写 `.env` 中保存的部署初始化令牌、创建管理员账户并同意协议。初始化后仍需保留 `.env`，因为 Compose 每次启动都会校验该必填变量。

3. 正式投入使用前请先加固：反向代理终结 TLS、TURN 接口与中继端口、各项 `LRD_*` 变量，都在[部署](/zh/guide/deployment)中说明。

4. 在信令服务器控制台的**信令接入令牌**页复制令牌。被控端按方式一下载运行，然后在它的**出站连接**设置页把信令服务器地址填成 `wss://<你的域名>/api/desk/signaling`，令牌填刚才复制的值。

::: warning
出于安全考虑，被控端默认拒绝以明文 `ws://` 连接**公网**信令地址（对应 [config.toml 参考](/zh/config/config-toml)中的 `require_secure_signaling`）。回环与内网 / 局域网地址不受此限制，因此纯内网、未配 TLS 的部署可以用 `ws://<VPS 地址>:8081/api/desk/signaling`。
:::

## 方式三：从源码运行（面向开发者）

### 前置条件

- 安装仓库钉定的 [Rust](https://www.rust-lang.org/) 工具链（Edition 2024，Rust 1.90）。
- 安装 [Node.js](https://nodejs.org/) 22.16 或更高版本。
- **AV1 编码（可选）**——在 Windows 上需要 [nasm](https://www.nasm.us/)：

  ```bash
  $NASM_VERSION="2.15.05"
  $LINK="https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/win64"
  curl --ssl-no-revoke -LO "$LINK/nasm-$NASM_VERSION-win64.zip"
  7z e -y "nasm-$NASM_VERSION-win64.zip" -o "C:\nasm"
  set PATH="%PATH%;C:\nasm"
  ```

平台相关的系统依赖（Linux / macOS）见[部署](/zh/guide/deployment)及项目的 `DEVELOPMENT_CN.md`。

### 先启动前端

Debug 构建的桌面外壳会加载 Vite 开发服务器，前端没起来就会白屏：

```bash
cd vite-project
npm ci
npm run dev
```

开发服务器监听 `http://localhost:5174`。

### 再启动被控端外壳

```bash
cargo run -p lcxl-remote-desk-tauri
```

它内嵌完整服务端，并额外提供防窥屏与白板。如果只需要纯后端而不要 GUI 外壳，改跑 `cargo run -p lcxl-remote-desk-server`，然后在浏览器访问 `http://localhost:5174`。

## 下一步

- 在[核心概念](/zh/guide/concepts)中了解各部分如何协作。
- 在[启动模式](/zh/guide/startup-modes)中理解不同进程布局。
- 通过 [config.toml 参考](/zh/config/config-toml)调整行为。
