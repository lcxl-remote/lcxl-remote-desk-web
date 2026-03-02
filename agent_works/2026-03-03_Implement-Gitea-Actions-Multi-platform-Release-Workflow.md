# Gitea Actions 多平台自动发布实现归档

**日期**: 2026-03-03
**标题**: Implement Gitea Actions Multi-platform Release Workflow

## 任务目标
在推送 `v*` 格式的 tag 时，自动触发 Gitea Actions 工作流，完成前端构建、后端多平台（Linux, Windows, macOS）编译、Tauri GUI 应用打包，并将所有产物发布到 Gitea Release。

## 方案设计
- **触发器**: `push tags: v*`
- **构建矩阵**: 包含 `ubuntu-latest`, `windows-latest`, `macos-latest`。
- **依赖管理**:
  - Linux: 使用 `apt` 安装 `libvpx-dev`, `libpipewire-0.3-dev` 等。
  - macOS: 使用 `brew` 安装 `libvpx`。
  - Windows: Rust 编译脚本自动处理 `libvpx`。
- **产物打包**:
  - Server 端：包含二进制、`conf`、`static` (前端静态资源)。
  - Tauri 端：使用 Tauri 官方构建流程输出安装包。

## 实现细节
已创建核心工作流文件：[.gitea/workflows/release.yaml](../../.gitea/workflows/release.yaml)

### 关键步骤说明
1. **Frontend Job**: 独立构建前端，利用缓存减少重复工作，产物通过 `upload-artifact` 传递给后续平台。
2. **Build (Server) Job**: 使用矩阵并行构建各平台的二进制程序。
3. **Tauri Job**: 并行构建各平台的桌面客户端安装包。
4. **Release Job**: 汇总所有 artifact 并调用 `softprops/action-gh-release` 发布。

## 验证与测试
- **语法验证**: YAML 文件格式符合 Gitea Actions 标准。
- **环境建议**: 
  > [!IMPORTANT]
  > 请确保 Gitea Runner 标签匹配：建议 Runner 拥有 `ubuntu-latest`, `windows-latest`, `macos-latest` 标签，或者在 YAML 中根据实际环境调整 `runs-on` 的值（例如改为 `linux`, `windows`）。

## 结论
本项目现已具备成熟的 CI/CD 发布能力，极大简化了新版本的发布流程。
