# 2026-04-14 - Native Remote Cursor Sync Implementation

## Background & Motivation
当前远程桌面的鼠标游标是直接由服务器绘制到捕获的视频帧上的。当网络条件不佳导致视频流卡顿或延迟时，用户在**控制**时会感觉鼠标不跟手、不流畅。为了提供 100% 流畅的鼠标控制体验，需要将远程鼠标的图形状态与视频流解耦。

具体行为规范如下：
1. **仅观看模式（无控制权）**：此时由于本地没有操作鼠标的需求，服务器应**继续**在本地将光标绘制到视频帧中，通过视频流传递给客户端，保持所见即所得。
2. **控制模式**：当客户端开始控制时，服务器**停止**将光标绘制到视频帧中。此时，如果远程鼠标的图形（形状）发生变化，服务器仅将其位图（Bitmap）及热点坐标（不包含屏幕坐标）发送给浏览器。浏览器接收后，将其作为动态缩放的 CSS `cursor` 应用在视频容器上，由于是本地渲染 CSS 样式，控制体验将绝对流畅。

## Scope & Impact
- **Backend (Rust)**：根据当前连接状态（是否处于控制中）动态切换光标绘制策略。在需要时提取各平台原生的鼠标图形并编码为 PNG，通过新的 WebRTC Data Channel 发送给客户端（不发送屏幕位置）。目前主要在 `web/server` 模块进行了更改。
- **Frontend (React)**：接收鼠标图形数据，利用离屏 `<canvas>` 将其等比例缩放（匹配当前远端桌面的真实分辨率），并通过 CSS `cursor: url(...)` 动态渲染。主要在 `web/vite-project` 模块进行了更改。
- **平台支持**：计划同时在 Windows、Linux 和 macOS 上实现原生鼠标图形提取。对于不支持原生提取的平台或协议，将平滑回退到“绘制到视频帧”的旧有方式。

## Implementation Plan & Task List

### 1. 数据通道与模型定义 (Backend)
- [x] 在 `web/server/src/model/data_channel.rs` 中增加新的常量 `DATA_CHANNEL_LABEL_CURSOR_SYNC_EVENT = "cursor_sync_event"`。
- [x] 定义新的通信模型 `CursorSyncData`，包含 `base64_png`, `hotspot_x`, `hotspot_y`, `visible`, `shape_id`，以及为高精度缩放新增的 `screen_width`, `screen_height` 属性。

### 2. 后端提取与状态控制逻辑 (Backend)
- [x] 在 `web/server/src/model/image_capture.rs` 中的 `ImageCapture` trait 添加 `capture_cursor(&mut self, last_shape_id: Option<u64>) -> Result<Option<CursorSyncData>, DeskError>` 方法。
- [x] 修改 `ImageCapture` 各平台的实现（如 `dxgi_capture.rs`, `gdi_capture.rs`）。获取鼠标图案、获取屏幕真实的物理像素分辨率 `screen_width`, `screen_height`，并处理图案组装成 `CursorSyncData`。
  - [x] 修复了 GDI 模式下光标消失标志位校验不完整导致光标不恢复的问题。
  - [x] 修复了 DXGI 模式下游标缓冲被误清空导致无法恢复的问题，以及剥离并修复了即使在控制模式下仍能正常检查更新鼠标形状的逻辑。
- [x] 在 `web/server/src/service/signaling.rs` 中，为 `PeerConnection` 结构体增加 `cursor_data_channel`。
- [x] 在 `signaling.rs` 的 `capture_screen_task` 中，动态判断当前平台的 `supports_native_cursor`。当有权控制且支持时，提取出图片数据，一旦 `shape_id` 变化或收到强制更新指令，经数据通道推向客户端。

### 3. 前端 WebRTC 集成 (Frontend)
- [x] 在 `web/vite-project/src/features/desk/use-desk-rtc.ts` 中，创建 `cursor_sync_event` 数据通道：
  - `cursorSyncChannel.current = pc.createDataChannel("cursor_sync_event", { ordered: true });`
- [x] 将 `cursorSyncChannel` 暴露在 Hook 的返回值中。

### 4. 前端动态光标缩放与渲染 (Frontend)
- [x] 创建新的 Hook `web/vite-project/src/features/desk/use-cursor-sync.ts`，接收 `cursorSyncChannel`, `videoRef` 以及 `hasControl` 参数。
- [x] 在 Hook 中监听 `message` 事件，解析 `CursorSyncData`。
- [x] 结合当前的“控制”状态：如果在控制模式，使用离屏 `<canvas>` 按当前视频容器的渲染比例计算真实缩放。利用后端传来的原视频宽 `videoOriginalWidth` 和原生屏幕宽 `screen_width`，避免在网络降级（WebRTC分辨率压缩）或高分屏（High-DPI，如 `window.devicePixelRatio > 1`）下造成的光标膨胀翻倍。直接交付不掺杂 CSS 倍率的原始换算尺寸。
- [x] 在 `desk-session.tsx` 中引入此 Hook，将生成的 `cursorStyle` 应用于 `<video>` 容器。

## Walkthrough (Execution Summary)
- 在原有基础上，通过 WebRTC Data Channel 进行原生鼠标图片的传输，解决远端视频流卡顿时鼠标操作的滞后感。
- **核心逻辑重构**：不仅从视频捕获模块内提取了 `PointerShapeBuffer` (DXGI) 或 HBITMAP (GDI) ，同时优化了服务端对 `shape_id` 的判定过滤逻辑，避免了高频转码操作，显著降低了后台 CPU 占用。
- **状态追踪修正**：精准化了光标生命周期。补齐了 GDI 下针对 `CURSOR_SHOWING` 标志位的掩码检测，剔除了 DXGI 中错误清理 `pointer_shape_buffer` 导致移动鼠标也无法恢复的漏洞。将提取光标形状动作与绘制光标动作解耦，确保即便是停止绘制到画面中，后端依然维持对鼠标图形的追踪。
- **完美 1:1 屏幕比例对齐**：彻底修复了 WebRTC 分辨率变动与高分屏 (High-DPI) 渲染导致游标巨大化的问题。通过直接读取物理 `DesktopCoordinates` 的宽高等信息进行回传，由前端利用物理真实宽度进行精确算子缩放。移除了浏览器自身针对 data URL 附带的逻辑像素拉伸影响。
- **全部代码构建通过（Cargo + TSC），无包含敏感信息。归档文件记录在 `agent_works/web/2026-04-14_native-remote-cursor-sync.md` 中。**