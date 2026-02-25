# Implementation Plan - Docker 构建集成 (Docker Build Integration)

本项目旨在创建一个 `Dockerfile` 和配套的构建脚本 `build.sh`，用于将前后端打包进一个极简的 Docker 镜像中。该镜像主要针对 `signal` 服务端运行。

## 用户审核 (User Review Required)

> [!NOTE]
> **已确认方案及最新反馈**:
>
> - **运行时镜像**: 使用 `debian:bookworm-slim`。
> - **构建加速**: 使用 Docker BuildKit 缓存挂载，使用 `.dockerignore` 过滤目录，并支持通过参数启用国内 Cargo 镜像源。
> - **StartupMode**: 修正启动参数为 `kebab-case`（如 `signaling` 而非 `Signaling`）。
> - **编排支持**: 增加 `docker-compose.yml` 及相关使用说明。

---

## 拟定变更 (Proposed Changes)

### 基础设施 (Infrastructure)

#### [NEW] [.dockerignore](file:///home/lcxl/code/lcxl-remote-desk-web/.dockerignore)

- 排除 `vite-project/node_modules`、`target`、`.git`、`logs` 等目录，减小构建上下文体积。

#### [MODIFY] [Dockerfile](file:///home/lcxl/code/lcxl-remote-desk-web/Dockerfile)

- **多阶段构建**:
  - `frontend-builder`: 基于 `node:20-slim`，使用 `npm ci` 构建 `vite-project`（启用 `ENABLE_MIRROR` 时使用 npmmirror 源）。
  - `rust-builder`: 基于 `rust:1.90-bookworm`，安装构建依赖（启用 `ENABLE_MIRROR` 时使用阿里云 apt 源并配置 Cargo 镜像），使用 `COPY . .` 复制整个项目。
  - `runtime`: 基于 `debian:bookworm-slim`（启用 `ENABLE_MIRROR` 时使用阿里云 apt 源）。
- **镜像源支持**: 引入 `ARG ENABLE_MIRROR`。如果启用，则配置以下镜像：
  - **Apt**: 将 Debian 源替换为 `mirrors.aliyun.com`。
  - **NPM**: 设置 registry 为 `https://registry.npmmirror.com`。
  - **Cargo**: 设置 source 为 `sparse+https://mirrors.aliyun.com/crates.io-index/`。

#### [MODIFY] [build.sh](file:///home/lcxl/code/lcxl-remote-desk-web/build.sh)

- **参数处理**:
  - 第一个参数为镜像 Tag，默认为 `lcxl/lcxl-remote-desk-web:latest`。
  - 增加 `--push` 标志位，支持构建后自动推送到镜像仓库。
- **构建逻辑**: 强制开启 `DOCKER_BUILDKIT=1`。
- **参数支持**: 增加 `--mirror` 参数。如果传入该参数，则在 `docker build` 命令中添加 `--build-arg ENABLE_MIRROR=true`。

#### [NEW] [docker-compose.yml](file:///home/lcxl/code/lcxl-remote-desk-web/docker-compose.yml)

- 定义 `remote-desk` 服务。
- 配置端口映射、容器挂载（`conf`, `logs`）。

### 文档更新 (Documentation)

#### [MODIFY] [README.md](file:///home/lcxl/code/lcxl-remote-desk-web/README.md)

- 增加 “Docker 使用” 章节，说明如何拉取（或构建）镜像并运行。
- 修正 `startup-mode` 的用法说明（使用 kebab-case）。
- 增加 `docker-compose` 使用说明。
- 增加 `--mirror` 参数的使用说明，帮助国内用户加速构建。

#### [MODIFY] [DEVELOPMENT.md](file:///home/lcxl/code/lcxl-remote-desk-web/DEVELOPMENT.md)

- 增加 “镜像开发” 章节，详细说明 `Dockerfile` 的构建逻辑、缓存机制以及如何使用 `build.sh` 进行本地开发构建。
- 增加 `.dockerignore` 的作用说明。
- 解释 `StartupMode` 的序列化规则。
- 说明如何利用 `--mirror` 解决下载缓慢问题。

---

## 验证方案 (Verification Plan)

### 自动验证

- 检查 `Dockerfile` 语法。
- 检查 `build.sh` 执行参数逻辑（默认值、`--push` 解析）。
- 检查 `Dockerfile` 和 `docker-compose.yml` 语法。

### 手动验证

- 执行 `./build.sh` 验证本地构建流程。
- 执行 `./build.sh my-tag:v1` 验证自定义 Tag。
- (可选) 执行 `./build.sh --push` 验证推送逻辑（需提前登录仓库）。
- 启动容器，访问 `http://localhost:8081` 检查前端静态资源加载及后端 API 响应。
- 执行 `./build.sh` 验证上下文过滤效果。
- 执行 `docker-compose up -d` 验证服务编排。
- 检查容器内启动进程的参数是否正确（kebab-case）。
