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
- `manager_enabled`——是否保持 manager 连接。留空（或 `true`）表示连接；设为 `false` 可在**不清空** `manager_url` / `manager_api_token` 的前提下断开 manager 链接，从而保留地址以便日后重新启用。可在**出站连接**设置页切换；这是被控端本机开关（manager 无法关闭自身链接）。
- `require_secure_signaling`——本机是否拒绝以明文协议（`ws://` / `http://`）连接**公网**信令服务器 / manager。留空（或 `true`，安全默认值）表示强制启用；仅当在可信网络中有意运行不带 TLS 的公网端点时，才设为 `false` 作为逃生阀。回环地址与私网 / 局域网目标（如 `192.168.x.x`、`127.0.0.1` 上自建的信令服务器）无论该开关如何都始终允许明文连接，且云元数据地址段始终被拦截。强制在连接时基于解析出的 IP 执行，因此解析到公网地址的域名也无法绕过。可在**出站连接**设置页切换。字段缺省时安全回落为 `true`。

> 当 manager 致命拒绝本机注册（设备数量已达上限，或本机缺少设备身份）时，desk-server 会暂停自动重连，**出站连接**设置页会显示横幅说明原因，并提供**重试注册**按钮。请先从任一控制端清理出一个设备名额，再重试。
- `local_signaling_token`——自动生成并持久化的令牌，供本地 desk server（及其他被控端）与同机信令服务器鉴权。请勿手动设置；它是凭据，日志中已脱敏。

### 本机远程访问展示

<code>host_access_indicator_enabled</code> 控制 Tauri 被控端是否显示持续远程访问状态卡、托盘活动徽标和首次会话通知，默认 <code>true</code>。关闭只隐藏这些提示，不改变审批、权限或已经建立的会话；该本机偏好不能由 manager 远程修改。见[被控端远程访问状态指示器](/zh/features/host-access-indicator)。

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
- `interfaces`——本机中继所用的地址（见[接口地址](#接口地址)）。
- `static_auth_secret`——静态鉴权密钥。
- `enable_turn`——是否在本机运行 TURN 服务（默认 `true`）。TURN 中继与 STUN 由同一个服务提供，关闭后两者都不再提供，连接只能依赖信令服务器提供的中继。
- `relay_min_port` / `relay_max_port`——中继端口分配范围。
- `[turn.static_credentials]`——可选的静态用户名 / 密码凭据表。

TURN 服务在运行期间会跟随以上配置：保存 TURN 设置页（或重新生成密钥）会立即重启该服务，**无需重启服务器**。**重启会中断当前正经该中继转发的连接**——它们会通过 ICE 仍能找到的其他候选重连；未走本机中继的会话不受影响。

若 `enable_turn = true` 但未配置任何 `interfaces`，则不会启动任何服务（没有可服务的地址）；运行状态接口会将其报告为 `not-configured`，与“被用户关闭”是两种不同的状态。

### 接口地址

每条配置包含 `transport`、`listen`（本机绑定的地址）与 `external`（告知对端去连的地址）：

```toml
[[turn.interfaces]]
transport = "udp"
listen = "0.0.0.0:3478"
external = "203.0.113.7:3478"

[[turn.interfaces]]          # IPv6 字面量必须带方括号
transport = "udp"
listen = "[::]:3478"
external = "[2001:db8::1]:3478"
```

两个地址都必须是 `IP:port` 形式。**不解析主机名**——请直接填地址。`external` 还必须是对端能够拨通的地址，因此通配地址（`0.0.0.0`、`::`）与 0 端口都会被拒绝。

只中继 UDP。`tcp` 条目既不监听也不广告，而不是广告出去却无人应答。

不满足上述要求的条目会被报告并跳过，其余条目照常提供服务。每条被拒的配置都会在启动日志中记录，并出现在运行状态接口的 `rejected_interfaces` 中，指明是哪一条、哪个字段、正确的写法是什么。若**全部**条目都被拒绝，则没有可服务的地址，状态为 `not-configured`——但会附带这些拒绝记录，这正是它与“一条接口都没配”的区别。

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

## 安全 `[security]`

对入站远程会话的按能力访问控制。每项能力为三态：未设置表示“每次询问本地用户”（文件默认），`true` 表示“始终允许”，`false` 表示“始终拒绝”。

初始化向导在安装时会写入一份显式姿态（为 owner 放开能力），因此经向导安装的被控端并非从“全询问”的文件默认起步。对于通过[访问码](/zh/guide/access-codes)兑换的非 owner 会话，这些全局设置还会与该码的能力上限及现场审批取 meet。

- `allow_remote_control`——鼠标 / 键盘输入。
- `allow_clipboard_sync`——剪贴板同步。
- `allow_private_screen`——防窥（隐私）屏模式。
- `allow_whiteboard`——白板叠加。
- `allow_terminal`——远程终端访问。
- `allow_file_browse`——列目录和查看文件元数据。
- `allow_file_delete`——文件删除；执行删除时还必须同时允许 `allow_file_browse`。
- `allow_file_transfer`——文件上传 / 下载。
- `approval_timeout`——授权提示框的等待时长（秒）。**默认 `30`**——超时后由被控端**服务端权威地取消（拒绝）该请求**，而不是让它永久挂起；该拒绝在服务端强制执行，即使授权界面已关闭或不可达也会兜底拒绝。设为 `0` 表示永不超时（提示框无限等待）。“从不”以数值 `0` 存储，因此重启后仍然保持。

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
