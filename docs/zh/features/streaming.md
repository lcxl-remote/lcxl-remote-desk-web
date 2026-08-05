# 远程控制与串流

LCXL Remote Desk 通过 **WebRTC** 串流远端屏幕与音频以实现超低延迟，并提供完整的键鼠控制。

## 视频编码

采集引擎支持跨多种编解码器的软硬件编码：

| 编解码器 | 后端 |
|---|---|
| H.264 | X264 / OpenH264 |
| VP8 / VP9 | libvpx |
| AV1 | SVT-AV1 |

采集后端：Windows（**DXGI / WGC**）、Linux（**X11 / Wayland portal + PipeWire**）。Linux 会按当前桌面会话筛选后端：原生 X11 会话只展示 X11，Wayland 会话只展示 ScreenCast Portal，避免把 XWayland 误认为完整桌面采集能力。

编码器可自动选择，或通过 `desk.video_encoder` 固定。帧率（`video_fps`）、画质（`video_quality`，0–63，越低越好）与脏矩形增量编码（`enable_dirty_rect`）均可配置——见 [config.toml 参考](/zh/config/config-toml#desktop-desk)。

所配置的编码器是**偏好**而非绝对选择：当控制端（浏览器、Android 或 iOS）连接时，被控端会把实际视频编码与该客户端在 WebRTC offer 中声明的可解码编码做协商。客户端能解码时优先采用所配置的编码器；否则被控端自动回退到双方都支持的最佳编码。由此客户端永远不会收到自己无法解码的编码，也无需为每种客户端单独配置能力。

## 音频

系统音频用 **Opus**（libopus）采集并编码。采集后端：Windows（**WASAPI**）、Linux（**ALSA / PipeWire**）。

## 输入

鼠标与键盘输入通过专用数据通道注入到被控设备。在 service-daemon 模式下，注入运行于用户桌面会话内——见[启动模式](/zh/guide/startup-modes)。

## 显示器选择

`desk.video_device_name` 选择要采集的显示器（如 Windows 上的 `\\.\DISPLAY1`）。首次连接时，浏览器会自动选择第一个包含可用显示器的采集模式，并优先选择桌面原点处的显示器；已保存的模式或显示器失效时会纠正到该可用默认值，并在对话框中说明变更。

## 调优建议

- 降低 `video_fps`（如 30）以减少 CPU 与带宽。
- 增大 `video_quality` 数值可减小码率，减小则画质更好。
- 屏幕大体静态时启用 `enable_dirty_rect`。
