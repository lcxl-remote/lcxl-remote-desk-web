# 贡献指南

欢迎贡献！本页总结工作流与编码规范。权威版本见仓库的 `CONTRIBUTING_CN.md`。

## 开发工作流

1. 配置工具链——Rust 1.90+ 与 Node.js 20+（系统依赖见[快速开始](/zh/guide/quick-start)与[部署](/zh/guide/deployment)）。
2. 用 `cargo run` 跑后端，在 `vite-project/` 用 `npm run dev` 跑前端。
3. 为你的改动添加测试——**每次代码改动都必须增加测试用例。**
4. 提交前格式化并 lint。

## 编码规范

### Rust

- 用 `rustfmt` 格式化；运行 `cargo clippy`。
- 函数/模块用 `snake_case`，类型用 `PascalCase`，常量用 `SCREAMING_SNAKE_CASE`。
- **注释必须用英文**，且只描述代码*当前*的行为——不留开发阶段标记。

### TypeScript / React

- 4 空格缩进；组件 `PascalCase`；hook `useXxx`；`components/ui` 下文件名 `kebab-case`。
- **国际化强制**——所有用户可见文本必须走 `t()`；每个新键须同时加入 `zh-CN` 与 `en-US` 语言文件。禁止硬编码字符串。
- `src/services/` 下的生成文件（Kubb 输出）禁止手动修改。

## 提交信息

- 使用英文并遵循 [Conventional Commits](https://www.conventionalcommits.org/)（`feat:`、`fix:`、`chore:` …）。

## 构建

```bash
# 后端
cargo build --release

# 前端
cd vite-project && npm run build
```

各部分位置及添加 API / 信令类型的分步配方见[模块地图](/zh/reference/modules)。
