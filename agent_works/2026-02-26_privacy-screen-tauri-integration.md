# 隐私屏功能 — Tauri 集成

## 背景

在远程桌面被控端实现「隐私屏」功能：远程控制时显示全屏置顶窗口覆盖被控端显示屏，拦截本地键鼠输入，仅允许控制端浏览器的输入生效。用 Tauri 替代原生 Windows 实现，支持跨平台（Windows/macOS/Linux），跑通整体信令链路。

## 实施概要

### Phase 1: Tauri 骨架 + Server 集成
- 创建 `tauri-app` workspace 成员
- 重构 `server/src/lib.rs`：新增 `ExternalChannels` 和 `run_with_channels()`
- Tauri `main.rs` 根据 `startup_mode` 决定是否启动窗口系统

### Phase 2: 隐私屏 Tauri 窗口
- `private_screen.rs`：创建/显示/隐藏全屏窗口，注册 Ctrl+Alt+L 退出快捷键
- 平台特定代码：
  - Windows: `WDA_EXCLUDEFROMCAPTURE` + `BlockInput`
  - macOS: `NSWindow.sharingType` + `CGEventTap`（TODO 实现）
  - Linux: X11 `XGrabKeyboard`/`XGrabPointer`

### Phase 3: 信令层端到端链路
- `signal-facade`：新增 `EnablePrivateScreen = 206`、`PrivateScreenStateChanged = 207`
- `DeskSession`：集成 `SystemSettingHelper`，处理隐私屏命令和状态推送
- 各平台 `SystemSettingHelper`：注入 `cmd_sender`，统一 `enable_private_screen(bool)` 接口
- 通过 `SignalingModel::new_request` + `DeskSessionMessage::Text` 推送状态变化

### Phase 4: 前端 UI
- `constants.ts`：新增信令常量
- `desk-session.tsx`：
  - 状态：`isPrivateScreen`、`isPrivateScreenSupported`
  - 信令监听：`PRIVATE_SCREEN_STATE_CHANGED` 更新 UI 状态
  - 工具栏按钮：ShieldCheck/ShieldOff 图标，仅在 `hasControl && isPrivateScreenSupported` 时显示
  - 处理函数：`handleTogglePrivateScreen` 发送 `EnablePrivateScreen` 信令

## 关键修改文件

| 文件 | 修改类型 |
|---|---|
| `server/src/lib.rs` | ExternalChannels + run_with_channels |
| `server/src/service/signaling.rs` | DeskSession 集成、信令处理 |
| `server/src/service/system_setting/*.rs` | cmd_sender 注入、enable_private_screen(bool) |
| `signal-facade/src/model/signal.rs` | 新增 SignalingType |
| `signal-facade/src/model/private_screen.rs` | 新增数据模型 |
| `vite-project/src/features/desk/constants.ts` | 新增常量 |
| `vite-project/src/features/desk/desk-session.tsx` | 隐私屏 UI 按钮 |

## 验证结果

- 后端 `cargo check`: 0 errors ✅
- 前端 `tsc --noEmit`: 无新增错误 ✅

## 待手动验证

- 端到端功能测试
- 被控端快捷键退出
- 被控端禁用隐私屏后控制端按钮隐藏
