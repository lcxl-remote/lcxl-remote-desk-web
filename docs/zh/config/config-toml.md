# config.toml 参考

服务端设置通过 `conf/config.toml` 管理。配置文件路径可用 `-c, --config-file-path <PATH>` 覆盖。

## 系统 `[system]`

- `enable_ipv6`——是否启用 IPv6 支持。
- `port`——服务端监听端口。
- `listen_addr_ipv4`——IPv4 监听地址。
- `listen_addr_ipv6`——IPv6 监听地址。
- `signaling_url`——要连接的独立信令服务器地址（留空则仅使用内置信令服务器）。
- `signaling_token`——远程信令服务器的节点接入令牌（以 `?token=` 附在信令 WebSocket 上）。
- `manager_url`——要连接的企业版 manager 信令地址。
- `manager_api_token`——manager 的接入令牌（以 `?token=` 附在 manager 信令 WebSocket 上）。
- `local_signaling_token`——自动生成并持久化的令牌，供本地 desk server（及其他被控端）与同机信令服务器鉴权。请勿手动设置；它是凭据，日志中已脱敏。

## 日志 `[log]`

- `log_level`——日志级别（`error`、`warn`、`info`、`debug`、`trace`）。
- `traceback`——是否启用 Rust 错误回溯。
- `log_retention_days`——日志保留天数（默认 `7`）。
- `log_cleanup_threshold_percent`——触发清理的磁盘占用阈值（默认 `90`）。
- `log_cleanup_interval_hours`——清理任务的间隔小时数（默认 `12`）。
- `tokio_console_enabled`——启用 tokio-console 订阅器（需 `tokio_unstable` 构建标志，默认 `false`）。

## 用户 `[user]`

- `login_user_name`——初始登录用户名。
- `login_password`——初始登录密码。

## TURN 服务 `[turn]`

- `realm`——用于鉴权的 TURN 服务 realm。
- `interfaces`——网络接口配置（`udp` / `tcp` 协议、监听与外部地址）。
- `static_auth_secret`——静态鉴权密钥。
- `enable_stun` / `enable_turn`——分别开关 STUN 与 TURN 中继。
- `relay_min_port` / `relay_max_port`——中继端口分配范围。
- `[turn.static_credentials]`——可选的静态用户名 / 密码凭据表。

## 桌面 `[desk]` {#desktop-desk}

- `video_fps`——视频帧率（默认 `60`）。降低可减少 CPU 与带宽占用。
- `video_quality`——视频编码质量（`0`–`63`，越低越好，默认 `22`）。
- `video_encoder` / `audio_encoder`——可选；省略时自动选择。视频可为 `X264` / `VP8` / `VP9` / `H264` / `AV1`；音频为 `OPUS`。
- `video_device_name`——要采集的显示器 GDI 设备名（`\\.\DISPLAYn`）；留空表示“首次连接时让浏览器选择”。
- `show_mouse`——是否采集并显示鼠标光标。
- `enable_dirty_rect`——是否启用脏矩形增量编码。
- `[desk.private_screen]`——防窥屏设置（`enabled` 等）。

## 虚拟显示器 `[virtual_display]` {#virtual-display-virtual-display}

- `enabled`——是否启用虚拟显示器（需已安装 IddCx 驱动；仅在特定模式下生效）。
- `exclusive` / `prompt_ms` / `adaptive_*`——独占模式与自适应分辨率参数。

## AI 设置

AI 供应商、base URL、模型与 API Key 通过**管理控制台**配置，而非 TOML 文件。API Key 是严格的服务端密钥。见 [AI 诊断](/zh/features/ai-diagnostics)。

## 推荐的开发配置

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # 开发期间降低 FPS 以节省资源
```
