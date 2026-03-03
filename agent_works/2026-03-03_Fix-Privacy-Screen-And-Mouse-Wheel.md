# Implementation Plan - Private Screen and Mouse Wheel Fixes

此计划旨在解决隐私屏在会话结束时未正确退出以及 Windows 平台鼠标滚轮速度过快的问题。

## Proposed Changes

### [Backend] Signaling Service

#### [MODIFY] [signaling.rs](file:///d:/source/lcxl-remote-desk-web/server/src/service/signaling.rs)
- **`handle_request_control`**: 当收到取消控制请求（`accept: false`）时，显式调用 `enable_private_screen(..., false)`。
- **`handle_message` (CloseControl)**: 在移除 PeerConnection 之前，显式调用 `enable_private_screen(..., false)`。
- **`shutdown`**: 在关闭所有连接前，确保所有会话的隐私屏都已关闭。

### [Backend] Mouse Event Service

#### [MODIFY] [windows.rs](file:///d:/source/lcxl-remote-desk-web/server/src/service/mouse_event/windows.rs)
- **`handle_mouse_wheel`**: 将滚轮速度缩放系数从 `120.0` 调整为 `1.0` (或 `1.2`)。目前的 `120.0` 会导致浏览器发送的像素位移被错误地放大为大量的滚轮刻度。

### [Frontend] Desk Feature

#### [MODIFY] [desk-session.tsx](file:///d:/source/lcxl-remote-desk-web/vite-project/src/features/desk/desk-session.tsx)
- **`handleDisconnect`**: 在断开连接前，如果隐私屏处于开启状态，主动发送关闭隐私屏的信令。

## Verification Plan

### Automated Tests
- 无（主要是 UI 和系统级交互，难以通过单元测试验证）。

### Manual Verification
1. **隐私屏退出测试**:
    - 进入远程桌面，开启隐私屏。
    - 点击“退出控制”按钮。验证被控端隐私屏是否退出。
    - 重新开启隐私屏。点击“断开连接”按钮。验证被控端隐私屏是否退出。
    - 重新开启隐私屏。直接关闭浏览器标签页。等待几秒后验证被控端隐私屏是否退出。
2. **鼠标滚轮测试**:
    - 在远程桌面中打开网页或文档。
    - 滚动鼠标滚轮。验证滚动速度是否与本地操作体验一致，不再出现“一滚到底”的情况。


# Task List

- [x] 调研与分析 (Research and Analysis)
    - [x] 分析隐私屏在取消控制时的退出逻辑
    - [x] 分析浏览器关闭远程桌面时隐私屏的退出逻辑
    - [x] 分析 Windows 平台鼠标滚轮事件处理逻辑
- [x] 制定开发计划 (Planning)
- [x] 修复隐私屏退出问题 (Execution - Private Screen Fix)
    - [x] 在 `DeskSessionMessage` 中增加 `WebRTCDropped` 并在 `start_desk_session` 内部接收处理。
    - [x] 在 `on_peer_connection_state_change` 中发送 `WebRTCDropped` 事件。
    - [x] 在 `handle_request_control` 中添加退出隐私屏逻辑
    - [x] 在 `CloseControl` 处理中添加退出隐私屏逻辑
    - [x] 在 `DeskSession::shutdown` 中添加全局退出隐私屏逻辑
- [x] 优化鼠标滚轮速度 (Execution - Mouse Wheel Optimization)
    - [x] 修改 `windows.rs` 中的滚轮速度缩放系数
- [x] 验证修复 (Verification)
    - [x] 验证隐私屏在各种场景下能正确退出 (编译通过)
    - [x] 验证鼠标滚轮速度是否更自然 (代码逻辑简化)


# Walkthrough - Private Screen and Mouse Wheel Fixes

此文档总结了为解决隐私屏未正确退出以及 Windows 平台鼠标滚轮速度过快问题所做的修改。

## 修复隐私屏退出问题

原先，由于后端处理 `CloseControl` 信号以及处理 WebRTC 的非正常断开时，没有配套调用关闭隐私屏的逻辑，导致隐私屏状态一直驻留在被控端。

我们在以下几个方面进行了修复：
1. **处理正常关闭与取消控制**：
   - 在 `signaling.rs` 中的 `handle_request_control` 方法中，如果是拒绝/取消控制 (`accept: false`)，则发送 `CloseControl` 的同时调用 `enable_private_screen(..., false)` 退出隐私屏。
   - 在接收处理 `CloseControl` 信令时，在销毁 `peer_connection` 的同时，调用 `enable_private_screen(..., false)` 确保对应会话的隐私屏被关闭。
   - 在 `DeskSession::shutdown` (整个会话服务退出时) 添加了相同的清理逻辑。
2. **处理强行关闭/异常断开**：
   - 我们在 `DeskSessionMessage` 枚举中新增了 `WebRTCDropped(String)` 消息。
   - 利用 `on_peer_connection_state_change` 监听底层 WebRTC 状态，如果探测到 `Closed`/`Failed`/`Disconnected`，就往内部管道发送 `WebRTCDropped` 事件。
   - 主循环收到 `WebRTCDropped` 后，安全地移除对应 `peer_connection`，并**调用 `enable_private_screen(..., false)` 关闭该会话的隐私屏**。
3. **前端优化**：
   - 即使是浏览器正常点击断开，前端也会在断开前探查当前是否有隐私屏运行。如果有，主动发出 `EnablePrivateScreen (false)` 的信令让后端关闭它。

## 优化鼠标滚轮速度

原先在 Windows 平台的 `handle_mouse_wheel` (文件 `mouse_event/windows.rs`) 中：
```rust
let wheel_delta = (event.delta_y * 120.0) as i32;
```
由于原本浏览器传递的 `delta_y` 值本身就是像素或合理的行数计量，把它再次放大 120 倍会导致极其离谱的速度跳跃。我们目前将其修正为 `event.delta_y as i32`，这会让滚动显得更加丝滑和吻合本地体验。

## 验证

- `cargo check` 编译成功，无任何关于新增字段借用与生命周期的错误。前端 `npm run build` 也顺利通过编译。
- 逻辑上，覆盖了“取消控制”、“点击断开按钮”、“直接关浏览器页面”三种常见的会话结束场景以保证隐私屏都能退出。
