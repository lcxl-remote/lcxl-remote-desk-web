# 快速开始

根据你的目标，有三种方式运行 LCXL Remote Desk。

## 方式一：Docker 部署（推荐）

使用 Docker Compose 启动服务：

```bash
docker-compose up -d
```

访问 `http://localhost:8081`，首次访问时设置管理员账户。

## 方式二：Tauri 桌面客户端

当你需要**防窥屏**或**白板**这类本地渲染增强功能时使用：

```bash
cd tauri-app
cargo tauri dev
```

## 方式三：从源码运行（面向开发者）

### 前置条件

- 安装最新稳定版 [Rust](https://www.rust-lang.org/)（Edition 2024，Rust 1.90+）。
- 安装 [Node.js](https://nodejs.org/) 20 或更高版本。
- **AV1 编码（可选）**——在 Windows 上需要 [nasm](https://www.nasm.us/)：

  ```bash
  $NASM_VERSION="2.15.05"
  $LINK="https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/win64"
  curl --ssl-no-revoke -LO "$LINK/nasm-$NASM_VERSION-win64.zip"
  7z e -y "nasm-$NASM_VERSION-win64.zip" -o "C:\nasm"
  set PATH="%PATH%;C:\nasm"
  ```

平台相关的系统依赖（Linux / macOS）见[部署](/zh/guide/deployment)及项目的 `DEVELOPMENT_CN.md`。

### 启动后端

信令与桌面服务默认启用：

```bash
cargo run --release
```

### 启动前端

```bash
cd vite-project
npm ci
npm run dev
```

访问 `http://localhost:5174`。

## 下一步

- 在[核心概念](/zh/guide/concepts)中了解各部分如何协作。
- 在[启动模式](/zh/guide/startup-modes)中理解不同进程布局。
- 通过 [config.toml 参考](/zh/config/config-toml)调整行为。
