# 远程控制与串流

LCXL Remote Desk 通过 **WebRTC** 串流远端屏幕与音频以实现超低延迟，并提供完整的键鼠控制。

## 视频编码

采集引擎支持跨多种编解码器的软硬件编码：

| 编解码器 | 后端 |
|---|---|
| H.264 | X264 / OpenH264 |
| VP8 / VP9 | libvpx |
| AV1 | SVT-AV1 |

采集后端：Windows（**DXGI / WGC**）、Linux（**X11 / Wayland portal + PipeWire**）。

编码器可自动选择，或通过 `desk.video_encoder` 固定。帧率（`video_fps`）、画质（`video_quality`，0–63，越低越好）与脏矩形增量编码（`enable_dirty_rect`）均可配置——见 [config.toml 参考](/zh/config/config-toml#desktop-desk)。

## 音频

系统音频用 **Opus**（libopus）采集并编码。采集后端：Windows（**WASAPI**）、Linux（**ALSA / PipeWire**）。

## 输入

鼠标与键盘输入通过专用数据通道注入到被控设备。在 service-daemon 模式下，注入运行于用户桌面会话内——见[启动模式](/zh/guide/startup-modes)。

## 显示器选择

`desk.video_device_name` 选择要采集的显示器（如 Windows 上的 `\\.\DISPLAY1`）。留空则在首次连接时让浏览器选择。

## 调优建议

- 降低 `video_fps`（如 30）以减少 CPU 与带宽。
- 增大 `video_quality` 数值可减小码率，减小则画质更好。
- 屏幕大体静态时启用 `enable_dirty_rect`。
