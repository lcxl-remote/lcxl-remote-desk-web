# 文档更新实施计划

本计划旨在整理项目文档结构，将开发相关的内容（技术栈、详细配置）从 `README` 移至 `DEVELOPMENT` 指南中，简化 `README` 内容，并确保中英文版本的一致性。

## 用户审核项 (User Review Required)

> [!IMPORTANT]
>
> - **功能列表优化**：建议从 `README` 的“功能描述”中移除“未实现”的项目，将其移至底部的“路线图 (Roadmap)”章节，避免误导新用户。
> - **英文版补全**：`DEVELOPMENT.md` (英文版) 目前内容较少，我将同步 `DEVELOPMENT_CN.md` 中的“命令行参数”、“调试技巧”、“常见问题”等章节。
> - **架构图同步**：统一中英文 README 中的 Mermaid 架构图，确保两者都包含 STUN/TURN 的完整路径。

## 拟议变更

### 1. 修改 README_CN.md

- [ ] 移除 `## 技术栈` (迁移至 DEVELOPMENT_CN.md)。
- [ ] 移除 `## 项目结构` (已在代码库中直观体现，或在开发指南中详细说明)。
- [ ] 移除 `## Docker 使用` 下的 `1. 执行构建脚本`。
- [ ] 简化 `## 配置说明`，仅保留核心说明并链接到 `DEVELOPMENT_CN.md`。
- [ ] 将“功能描述”中未实现的功能移至新章节 `## 路线图`。
- [ ] 统一 Mermaid 架构图，补全 STUN 节点说明。

### 2. 修改 DEVELOPMENT_CN.md

- [ ] 在 `## 环境要求` 后插入从 README 迁移来的 `## 技术栈`。
- [ ] 在 `## 开发配置详解` 中整合详细的配置文件参数说明。
- [ ] 确保 `## 命令行参数` 章节准确无误。

### 3. 修改 README.md (同步中文版变更)

- [ ] 同步移除 `## Tech Stack` 和 `## Project Structure`。
- [ ] 同步移除 Docker 部分的 `1. Execute Build Script`。
- [ ] 简化 `## Configuration` 并链接到 `DEVELOPMENT.md`。
- [ ] 同步功能列表变更为 Features + Roadmap 结构。
- [ ] 统一 Mermaid 架构图。

### 4. 修改 DEVELOPMENT.md (对齐中文版)

- [ ] [NEW] 插入 `## Tech Stack`。
- [ ] [MODIFY] 扩展 `## Configuration Details`，对齐中文版的详细列表。
- [ ] [NEW] 同步 `## CLI Arguments` 章节。
- [ ] [NEW] 同步 `## Debugging Tips` 章节。
- [ ] [NEW] 同步 `## FAQ` 章节。

## 验证计划

### 手动验证

- 检查修改后的 `.md` 文件在预览中的格式是否正确。
- 验证文档间的跳转链接是否依然有效。
- 确保中英文档内容严格对应，术语翻译准确。

### Git 提交

- 使用英文提交信息：`docs: restructure README and DEVELOPMENT files for better clarity and alignment`

---
# 文档优化工作总结

针对项目文档（README 和 DEVELOPMENT 指南）的结构调整和内容对齐工作已完成。

## 变更说明

### 1. README (中英文版)

- **迁移与移除**：移除了 `## 技术栈` 和 `## 项目结构`（迁移至开发指南）。
- **简化**：简化了 Docker 构建部分的说明，移除了冗长的脚本参数，仅保留运行示例。
- **功能列表重组**：将未实现功能移至 `## 路线图 (Roadmap)`，`## 功能描述 (Features)` 仅保留已实现功能。
- **架构图更新**：统一并补全了 Mermaid 网络架构图，增加了 STUN 服务器节点说明。

### 2. 开发指南 (中英文版)

- **内容整合**：整合了从 README 迁移来的技术栈和详细配置参数说明。
- **英文版补全**：大幅扩充了 `DEVELOPMENT.md`，现在其包含与中文版完全对应的所有章节（CLI 参数、调试技巧、FAQ等）。
- **规范修复**：修复了 `DEVELOPMENT_CN.md` 中的二级标题重复问题，优化了目录及其对应的锚点链接。

## 验证结论

- 所有 `.md` 文件的结构符合实施计划。
- 中英文文档内容高度一致，翻译符合语境。
- 内部链接（如 `[开发指南](DEVELOPMENT.md)`）和页内锚点链接（如 `(#configuration-details)`）已通过手动核对或修正。

## 提交记录

- **Commit Message**: `docs: restructure README and DEVELOPMENT files for better clarity and alignment`
