# 归档：优化信令服务代码组织结构

## 任务描述
`server/src/service/signaling.rs` 文件过大（接近 2300 行），包含了大量不属于信令范畴的业务逻辑（如终端管理、文件管理）。需要将这些逻辑抽离到专门的模块中，以提高代码的可维护性和内聚性。

## 实现计划 (Implementation Plan)
1. **终端管理相关逻辑的抽离**：将 `RunningTerminal` 结构体、终端启动、数据传输、调整大小、关闭等逻辑迁移到 `server/src/service/terminal.rs`。
2. **文件管理相关逻辑的抽离**：将文件列表获取和文件删除的信令处理逻辑迁移到 `server/src/service/file_manager.rs`。
3. **信令服务瘦身**：在 `signaling.rs` 中通过委托模式调用上述抽离的方法，并清理不再需要的引用和结构定义。

## 执行总结 (Walkthrough)
1. **迁移终端逻辑**：
   - 在 `terminal.rs` 中引入必要的 PTY 和信令相关依赖。
   - 迁移 `RunningTerminal` 结构体和 `force_kill_terminal_process` 函数。
   - 重构终端处理函数（`handle_manager_terminal_start` 等），使其接收 `&mut DeskSession`。
2. **迁移文件管理逻辑**：
   - 在 `file_manager.rs` 中引入信令处理所需的依赖。
   - 迁移 `handle_manager_file_list` 和 `handle_manager_file_delete` 函数。
3. **重构信令入口**：
   - 修改 `signaling.rs` 中的 `DeskSession` 结构体，使用跨模块的 `RunningTerminal` 定义。
   - 将 `DeskSessionSender` 的 `sender` 字段设为 `pub`，确保跨模块可见。
   - 在 `handle_message` 的 `match` 分支中，将原有的本地方法调用替换为对 `terminal` 和 `file_manager` 模块的静态方法调用。
4. **清理与验证**：
   - 移除 `signaling.rs` 中冗余的 `import`。
   - 执行 `cargo check` 确保所有模块间引用正确，无编译错误。

## 归档时间
2026-03-17
