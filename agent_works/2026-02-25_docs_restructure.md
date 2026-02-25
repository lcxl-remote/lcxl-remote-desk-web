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

- [x] 移除 `## 技术栈` (迁移至 DEVELOPMENT_CN.md)。
- [x] 移除 `## 项目结构` (已在代码库中直观体现，或在开发指南中详细说明)。
- [x] 移除 `## Docker 使用` 下的 `1. 执行构建脚本`。
- [x] 简化 `## 配置说明`，仅保留核心说明并链接到 `DEVELOPMENT_CN.md`。
- [x] 将“功能描述”中未实现的功能移至新章节 `## 路线图`。
- [x] 统一 Mermaid 架构图，补全 STUN 节点说明。

### 2. 修改 DEVELOPMENT_CN.md

- [x] 在 `## 环境要求` 后插入从 README 迁移来的 `## 技术栈`。
- [x] 在 `## 开发配置详解` 中整合详细的配置文件参数说明。
- [x] 确保 `## 命令行参数` 章节准确无误。

### 3. 修改 README.md (同步中文版变更)

- [x] 同步移除 `## Tech Stack` 和 `## Project Structure`。
- [x] 同步移除 Docker 部分的 `1. Execute Build Script`。
- [x] 简化 `## Configuration` 并链接到 `DEVELOPMENT.md`。
- [x] 同步功能列表变更为 Features + Roadmap 结构。
- [x] 统一 Mermaid 架构图。

### 4. 修改 DEVELOPMENT.md (对齐中文版)

- [x] [NEW] 插入 `## Tech Stack`。
- [x] [MODIFY] 扩展 `## Configuration Details`，对齐中文版的详细列表。
- [x] [NEW] 同步 `## CLI Arguments` 章节。
- [x] [NEW] 同步 `## Debugging Tips` 章节。
- [x] [NEW] 同步 `## FAQ` 章节。

---

# 阶段一：文档结构重构工作总结

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

---

# 阶段二：开源准备增强工作总结

项目已完成针对开源发布的一系列标准化整备与隐私优化工作。

## 变更说明

### 1. 社区规范与隐私优化

- **[NEW] CONTRIBUTING_CN.md / CONTRIBUTING.md**: 建立了中英文贡献指南，统一沟通渠道为 GitHub Issue。
- **[NEW] SECURITY_CN.md / SECURITY.md**: 建立了中英文安全政策。根据用户反馈，已**移除所有邮件地址**，改为引导用户使用 GitHub 原生的“[私密漏洞报告 (Private Vulnerability Reporting)](https://github.com/lcxl/lcxl-remote-desk-web/security/advisories/new)”功能，有效保护开发者隐私。

### 2. GitHub 协作模板

- **[NEW] .github/ISSUE_TEMPLATE/**: 部署了 Bug Report 和 Feature Request 模板。
- **[NEW] .github/pull_request_template.md**: 部署了 PR 自测模板。

### 3. 文档体系补全

- 完成了 README 与 DEVELOPMENT 手册的高质量中英文对齐。
- 修复了所有文档内的跳转链接与锚点。

## 验证结论

- 经多次核查，代码及文档中已不含硬编码的个人邮箱及敏感信息。
- GitHub 维护模板格式正确。

## 提交记录

- **Commit Message**: `chore: finalize open source readiness and privacy optimization`

> [!IMPORTANT]
> **后续操作**：请确保在 GitHub 仓库的 `Settings -> Code security and analysis` 中开启 `Private vulnerability reporting` 功能。
