# 贡献指南

感谢你对 LCXL Remote Desk Web 的关注！我们欢迎各种形式的贡献，包括修复 bug、改进文档、提出新功能建议或提交代码。

## 如何参与

### 1. 报告 Bug

- 在提交 Issue 之前，请先搜索是否已有类似的 Issue。
- 使用项目提供的 Bug Report 模板。
- 请尽可能详细地描述问题，包括复现步骤、详细的日志、环境信息等。

### 2. 提交功能建议

- 使用 Feature Request 模板描述你的构思。
- 说明该功能的应用场景和价值。

### 3. 提交代码 (Pull Requests)

- **分支规范**：请基于 `main` 分支创建功能分支（例如 `feature/your-feature-name` 或 `fix/your-bug-fix`）。
- **代码风格**：
  - 后端 Rust 代码必须通过 `cargo fmt` 格式化，并且无 `cargo clippy` 警告。
  - 前端代码需遵循 ESLint 规范。
- **提交记录**：请使用清晰的英文描述提交内容，建议遵循 [Conventional Commits](https://www.conventionalcommits.org/) 规范。
- **测试**：如果可能，请为你的更改添加相应的单元测试或集成测试。

## 开发规范

详细的开发环境配置和流程请参考 [开发指南](DEVELOPMENT_CN.md)。

## 联系方式

如有任何疑问或需要进一步讨论，请通过 Issue 与维护者取得联系。
