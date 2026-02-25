# 文档更新记录 (2026-02-25)

## 1. 实施计划 (Implementation Plan)

### 主要变更点
- **前端工具**: 从 UmiJS Max/Ant Design Pro 迁移到 Vite + React + TailwindCSS + Shadcn UI。
- **自动生成代码**: 提及使用 Kubb 基于 OpenAPI 自动生成前端客户端。
- **构建与部署**: 更新 Dockerfile 构建逻辑及 build_docker.sh 脚本说明。
- **依赖项**: 补充 Linux 构建所需的 libvpx-dev, libclang-dev, cmake 等。

## 2. 任务清单 (Task List)
- [x] 调研项目现状
- [x] 制定更新计划
- [x] 执行内容更新
- [x] 验证链接与格式

## 3. 完工说明 (Walkthrough)

### [README.md](README.md) 更新项：
- **技术栈**: 前端描述更新为 Vite 7 + React 19 + TailwindCSS。
- **项目结构**: 替换 static/ 为 vite-project/。
- **Docker**: 优化 Docker 使用说明及镜像加速。

### [DEVELOPMENT.md](DEVELOPMENT.md) 更新项：
- **依赖补充**: 添加 libx11-dev, libxcb1-dev, libclang-dev, cmake。
- **开发流程**: 详述 vite-project 目录下的开发命令。
- **常见问题**: 更新 WebRTC 连接失败及信令模式说明。

### 验证结果
- 所有目录及命令均已验证。
- 修正了 Docker 构建中前端产物复制路径的说明。
