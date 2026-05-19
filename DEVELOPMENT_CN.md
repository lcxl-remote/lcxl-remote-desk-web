# 开发指南

[English](DEVELOPMENT.md)

本文档提供 LCXL Remote Desk Web 项目的完整开发指南，包括环境配置、开发流程、API 文档和代码规范。

## 目录

- [环境要求](#环境要求)
- [快速开始](#快速开始)
- [开发配置详解](#开发配置详解)
- [API 文档](#api-文档)
- [开发指南](#开发实操指南)
- [代码规范](#代码规范)

## 环境要求

### 技术栈

#### 后端

- **语言**: Rust (Edition 2024, Rust 1.90+)
- **Web 框架**: Actix-Web 4.11
- **WebRTC**: webrtc-rs 0.13
- **会话管理**: Actix-Session with Cookie
- **日志**: env_logger 0.11
- **配置管理**: config 0.15 (TOML)
- **API 文档**: Utoipa 5 (支持 Swagger, Redoc, RapiDoc, Scalar)
- **TURN 服务**: turn-server 3.4
- **Prometheus 监控**: Prometheus 0.13.4

#### 前端

- **框架**: React 19
- **UI 组件**: TailwindCSS + Shadcn UI (Radix UI)
- **构建工具**: Vite 7
- **代码生成**: Kubb (OpenAPI to React Query/TypeScript)
- **语言**: TypeScript 5.9
- **终端模拟**: xterm.js 5.5
- **状态管理**: TanStack Query (React Query) v5

#### 多媒体处理

- **视频捕获**: Windows (DirectX), Linux (X11RB)
- **视频编码**: VP8, VP9 (libvpx)
- **音频捕获**: Windows (WASAPI), Linux (ALSA, PipeWire)
- **音频编码**: Opus (libopus)

### 系统环境

### Rust 开发环境

- Rust 1.90 或更高版本
- Cargo (随 Rust 一起安装)

### Node.js 前端开发

- Node.js 12.0.0 或更高版本
- npm 或 yarn 或 pnpm

### Linux (包括WSL)系统依赖

```bash
sudo apt install -y build-essential
sudo apt install -y pkg-config
sudo apt install -y libssl-dev
sudo apt install -y libasound2-dev
sudo apt install -y libpipewire-0.3-dev
sudo apt install -y libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev
sudo apt install -y clang libclang-dev
sudo apt install -y cmake
sudo apt install -y libvpx-dev
```

### Windows 系统

- 无需额外依赖，项目通过 Cargo 自动管理

## 快速开始

### 1. 克隆仓库

```bash
git clone <repository-url>
cd lcxl-remote-desk-web
```

### 2. 后端开发

#### 配置服务器

编辑 `conf/config.toml` 文件，根据需要调整配置：

```toml
[system]
enable_ipv6 = true          # 是否启用 IPv6
port = 8081                  # 服务器端口
listen_addr_ipv4 = "0.0.0.0" # IPv4 监听地址
listen_addr_ipv6 = "::"       # IPv6 监听地址
log_level = "debug"          # 日志级别

[user]
login_user_name = "admin"    # 登录用户名
login_password = "admin"     # 登录密码

[desk]
video_fps = 60               # 视频帧率
video_encoder = "VP8"        # 视频编码器 (VP8/VP9)
audio_encoder = "OPUS"       # 音频编码器
```

#### 构建并运行服务器

```bash
cargo build --release
cargo run --release
```

或使用 cargo 直接运行：

```bash
cargo run
```

#### 访问 Web 界面

打开浏览器访问：`http://localhost:8081`

默认登录凭据：

- 用户名: `admin`
- 密码: `admin` (首次启动时会自动生成随机密码)

### 3. 前端开发

#### 进入前端目录

```bash
cd vite-project
```

#### 安装依赖

```bash
npm install
```

#### 启动开发服务器

```bash
npm run dev
```

#### 构建生产版本

```bash
npm run build
```

## 开发配置详解

### 服务器配置 (conf/config.toml)

#### 系统设置 [system]

- `enable_ipv6`: 是否启用 IPv6 支持
- `port`: 服务器监听端口
- `listen_addr_ipv4`: IPv4 监听地址
- `listen_addr_ipv6`: IPv6 监听地址
- `log_level`: 日志级别 (error, warn, info, debug, trace)
- `traceback`: 是否启用 Rust 错误回溯

#### 用户设置 [user]

- `login_user_name`: 登录用户名
- `login_password`: 登录密码

#### TURN 服务器 [turn]

- `realm`: TURN 服务器域，用于身份验证。
- `interfaces`: 网络接口配置。支持 `udp` 和 `tcp` 协议，及监听端口。
- `static_credentials`: 静态凭据配置，包含 `user` 和 `password`。

#### 桌面设置 [desk]

- `video_fps`: 视频帧率 (默认 60)。降低此值可减少 CPU 和带宽占用。
- `video_encoder`: 视频编码器。建议根据硬件支持选择 `VP8` 或 `VP9`。
- `audio_encoder`: 音频编码器。目前主要支持 `OPUS`。
- `video_device_name`: 指定要捕获的显示器的 GDI 设备名（`\\.\DISPLAYn`）；为空串时由浏览器在首次连接时弹框选择。
- `show_mouse`: 是否在远程画面中捕捉并显示移动的鼠标指针。

### 开发模式推荐配置

```toml
[system]
log_level = "debug"          # 开发时使用 debug 级别日志
traceback = true             # 启用错误回溯

[desk]
video_fps = 30               # 开发时可降低帧率以减少资源消耗
```

## API 文档

### 访问 API 文档

服务器启动后，可以通过以下 URL 访问 API 文档：

- **Swagger UI**: <http://localhost:8081/swagger-ui/>
- **ReDoc**: <http://localhost:8081/redoc>
- **RapiDoc**: <http://localhost:8081/rapidoc>
- **Scalar**: <http://localhost:8081/scalar>

API 规范定义：<http://localhost:8081/openapi.json>

### API 端点

#### 认证相关

- `POST /api/desk/login`: 用户登录
- `POST /api/desk/logout`: 用户登出
- `POST /api/desk/captcha`: 获取验证码
- `POST /api/desk/password/change`: 修改密码

#### 桌面控制

- `GET /api/desk/info`: 获取系统信息
- `GET /api/desk/settings`: 获取设置
- `POST /api/desk/settings`: 更新设置

#### 文件传输

- `GET /api/desk/files`: 列出文件
- `DELETE /api/desk/files`: 删除文件

#### 终端控制

- `GET /api/desk/terminal`: 列出终端会话
- `POST /api/desk/terminal/open`: 打开终端会话

#### WebRTC 信令

- `GET /api/desk/signaling`: 建立 WebSocket 信令连接

#### TURN 服务器

- `GET /api/turn/info`: 获取 TURN 服务器信息
- `GET /api/turn/sessions`: 获取 TURN 会话列表
- `DELETE /api/turn/sessions`: 删除 TURN 会话
- `GET /api/turn/metrics`: 获取 TURN 统计指标

## 开发实操指南

### 项目架构

项目采用模块化设计，主要分为以下几个部分：

- **server/**: 主服务器应用
  - **controller/**: 处理 HTTP 请求和路由
  - **model/**: 数据模型定义
  - **service/**: 业务逻辑实现
- **signal/**: WebRTC 信令服务 (及内置 TURN)
- **vite-project/**: React + Vite 前端应用
- **utils/**: 公共工具类库

### 添加新功能

1. **控制器 (Controller)**: 在 `server/src/controller/` 中添加路由处理器
2. **模型 (Model)**: 在 `server/src/model/` 中定义数据结构
3. **服务 (Service)**: 在 `server/src/service/` 中实现业务逻辑

#### 示例：添加新的 API 端点

1. 在 `server/src/model/` 中定义请求和响应模型
2. 在 `server/src/service/` 中实现业务逻辑
3. 在 `server/src/controller/` 中创建路由处理函数
4. 在 `server/src/main.rs` 中注册新的路由

## 代码规范

### Rust 后端

- Rust 代码遵循 `rustfmt` 格式化
- 使用 `cargo clippy` 进行代码检查

运行格式化和检查：

```bash
# 格式化代码
cargo fmt

# 代码检查
cargo clippy

# 运行测试
cargo test
```

### 前端 (TypeScript/React)

- 前端代码遵循 ESLint 和 Prettier 规范
- 组件采用函数式组件 + Hooks 模式

运行格式化和检查：

```bash
cd vite-project

# ESLint 检查
npm run lint

# 如果配置了 prettier
# npm run prettier
```

## 调试技巧

### 后端调试

1. **日志级别**：在 `config.toml` 中设置 `log_level = "debug"` 或 `"trace"`
2. **环境变量**：使用 `RUST_LOG=debug cargo run` 覆盖日志级别
3. **错误回溯**：在 `config.toml` 中设置 `traceback = true`

### 前端调试

1. **开发服务器**：运行 `npm start` 启动带热重载的开发服务器
2. **浏览器开发工具**：使用 Chrome/Firefox DevTools 调试
3. **React DevTools**：安装 React 浏览器扩展进行组件调试

## 镜像构建与部署 (Docker)

### 构建生产版本

#### 后端

```bash
cargo build --release
```

生成的二进制文件位于 `target/release/`

#### 前端

```bash
cd vite-project
npm run build
```

生成的静态文件位于 `vite-project/dist/`，在 Docker 构建中会自动复制到 `server/static` 供后端嵌入。

### 镜像开发 (Docker Development)

项目支持通过 Docker 进行容器化部署和开发。

#### 1. 构建逻辑

`Dockerfile` 采用了三阶段构建以优化镜像大小：

- **frontend-builder**: 基于 `node:20-slim`，利用 `npm` 缓存挂载构建前端。
- **rust-builder**: 基于 `rust:1.90-bookworm`，安装 C 库依赖，并将前端产物内置到 `server/static` 进行后端编译。
- **runtime**: 基于 `debian:bookworm-slim`，精简运行环境。

#### 2. 构建加速

构建过程使用了 Docker BuildKit 的缓存机制：

- `mount=type=cache,target=/root/.npm`: 缓存 npm 依赖。
- `mount=type=cache,target=/usr/local/cargo/registry`: 缓存 Cargo 注册表。
- `mount=type=cache,target=/app/target`: 缓存 Rust 编译产物。

#### 3. 使用构建脚本

项目根目录提供了 `build_docker.sh` 方便本地构建：

```bash
# 构建默认 Tag (lcxl/lcxl-remote-desk-web:latest)
./build_docker.sh

# 构建后推送到镜像中心
./build_docker.sh --push
```

#### 4. 使用 Docker Compose

推荐开发环境下使用 `docker-compose` 进行快速部署验证：

```bash
docker-compose up --build -d
```

注意：构建时必须确保本地 Docker 版本支持 BuildKit（脚本已自动设置 `DOCKER_BUILDKIT=1`）。

### 打包分发

完整的应用包括：

- 后端可执行文件
- `conf/` 配置目录
- `static/dist/` 前端静态文件

## 命令行参数

```bash
cargo run -- --help
```

可用参数：

- `-c, --config-file-path <PATH>`: 配置文件路径 (默认: conf/config)
- `-m, --startup-mode <MODE>`: 启动模式
  - `default`: 默认模式，包含信令和桌面服务器
  - `signaling`: 仅信令模式 (信令 + TURN)
  - `desk-server`: 仅桌面服务器

## 常见问题

### 编译错误

**问题**：找不到系统依赖库
**解决**：确保已安装所有必需的系统依赖（参见[环境要求](#环境要求)）

**问题**：Rust 版本过旧
**解决**：运行 `rustup update` 更新 Rust 到最新版本

### 运行时错误

**问题**：端口被占用
**解决**：在 `config.toml` 中修改 `port` 配置

**问题**：WebRTC 连接失败
**解决**：检查 STUN/TURN 服务器配置，确保网络连接正常。如果是外网访问，请确保信令服务器模式已正确启动并映射了相应端口。

## 贡献指南

欢迎贡献代码！请遵循以下流程：

1. Fork 项目仓库
2. 创建功能分支 (`git checkout -b feature/AmazingFeature`)
3. 提交更改 (`git commit -m 'Add some AmazingFeature'`)
4. 推送到分支 (`git push origin feature/AmazingFeature`)
5. 开启 Pull Request

提交前请确保：

- [ ] 代码通过 `cargo fmt` 和 `cargo clippy` 检查
- [ ] 前端代码通过 ESLint 检查
- [ ] 添加了必要的测试
- [ ] 更新了相关文档
