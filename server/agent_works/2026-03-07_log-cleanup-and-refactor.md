# 2026-03-07 日志自动清理机制实现与异步重构

## 1. 任务概述

本项目成功实现了日志自动清理机制，支持基于保留天数和磁盘空间使用率的后台自动化管理，并提供了前端配置界面。同时，对 `telemetry` 初始化逻辑进行了异步化重构，优化了系统配置的共享机制。

## 2. 任务列表进度

- [x] 构思并编写实现方案
- [x] 完善 `SystemSettings` 结构体，添加清理配置项
- [x] 在 `telemetry.rs` 中实现带磁盘检查的定期清理逻辑
- [x] 确保配置项可通过 API/前端修改
- [x] 验证清理机制（时间维度与磁盘空间维度）
- [x] 重构 `init_telemetry` 为异步方法并移除 `block_on`
- [x] 归档任务与 Git 提交

## 3. 实现详情

### 后端变更

- **[settings.rs](file:///home/lcxl/code/lcxl-remote-desk-web/server/src/model/settings.rs)**:
  - 在 `SystemSettings` 中增加了 `log_retention_days` (默认 7), `log_cleanup_threshold_percent` (默认 90%), `log_cleanup_interval_hours` (默认 12) 字段。
- **[telemetry.rs](file:///home/lcxl/code/lcxl-remote-desk-web/server/src/telemetry.rs)**:
  - 实现了 `spawn_log_cleanup_task` 后台异步任务。
  - 将 `init_telemetry` 重构为 **异步方法 (async)**，消除了 `block_on` 调用。
  - 实现了 `perform_log_cleanup` 核心逻辑：
    1. **时间过滤**: 自动删除超过保留天数的日志。
    2. **磁盘空间检查**: 若磁盘空间占比超过阈值，则按日期排序删除旧日志。
  - 修正了 `sysinfo` 0.37+ 的 `Disks` API 调用方式。
- **[lib.rs](file:///home/lcxl/code/lcxl-remote-desk-web/server/src/lib.rs)**:
  - **统一配置实例**: 修复了 `SharedSettings` 重复实例化的逻辑问题，确保全局复用同一个 `Arc<SharedSettings>` 实例。
  - 配合 `telemetry::init_telemetry` 的异步化，增加了 `.await` 调用。
- **[openapi.rs](file:///home/lcxl/code/lcxl-remote-desk-web/server/src/openapi.rs)**:
  - 清理了冗余的手工 `SystemSettings` 定义。

### 前端变更

- **[system-settings.tsx](file:///home/lcxl/code/lcxl-remote-desk-web/vite-project/src/features/settings/system-settings.tsx)**:
  - 增加了“日志清理 (Log Cleanup)”配置区块。
  - **规整国际化 Key**: 将 Key 修改为规范的 camelCase 命名（如 `logRetentionDays`）。
- **国际化翻译**:
  - **[zh-CN/pages.ts](file:///home/lcxl/code/lcxl-remote-desk-web/vite-project/src/locales/zh-CN/pages.ts)** & **[en-US/pages.ts](file:///home/lcxl/code/lcxl-remote-desk-web/vite-project/src/locales/en-US/pages.ts)**: 补全了日志清理相关的中英文翻译条目。

## 4. 验证结论

- **逻辑验证**: 清理任务能正确识别 `desk-server.log.YYYY-MM-DD` 格式的日志。
- **磁盘监控**: 使用 `sysinfo` 实时监控空间占比。
- **编译状态**: 后端代码已通过 `cargo check` 验证。
- **UI/i18n**: 前端配置界面已适配多语言并遵循命名规范。
