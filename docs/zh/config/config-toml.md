# config.toml 参考

服务端设置使用与启动模式无关的平台默认路径：Windows 为 `%ProgramData%\LCXL Remote Desktop\config\config.toml`；Linux 普通用户为 `XDG_CONFIG_HOME/lcxl-remote-desk/config.toml`（未设置时 `~/.config/lcxl-remote-desk/config.toml`），root 为 `/etc/lcxl-remote-desk/config.toml`；macOS 为 `~/Library/Application Support/com.lcxl.remote-desk/config/config.toml`。`-c, --config-file-path <PATH>` 用于显式选择其他 profile；只有显式传值时服务或 LaunchAgent 才继承该路径。系统不会自动发现或迁移旧的 cwd 相对 `conf/config.toml`。

## 系统 `[system]`

- `enable_ipv6`——是否启用 IPv6 支持。
- `port`——服务端监听端口。
- `listen_addr_ipv4`——IPv4 监听地址。
- `listen_addr_ipv6`——IPv6 监听地址。
- `signaling_url`——要连接的独立信令服务器地址（留空则仅使用内置信令服务器）。
- `signaling_token`——远程信令服务器的节点接入令牌（以 `?token=` 附在信令 WebSocket 上）。
- `manager_url`——要连接的企业版 Manager 信令地址。
- `manager_api_token`——Manager 接入令牌（以 `?token=` 附在 Manager 信令 WebSocket 上）。
- `manager_enabled`——是否保持 Manager 连接。留空或设为 `true` 表示保持连接；设为 `false` 可以在**保留** `manager_url` 和 `manager_api_token` 的情况下断开连接，方便以后重新启用。可在**出站连接**设置页面切换；这是被控端的本机开关，Manager 无法远程开启或关闭它。
- `require_secure_signaling`——本机是否拒绝通过明文协议（`ws://` 或 `http://`）连接**公网**信令服务器或 Manager。留空或设为 `true` 时强制使用加密连接，也是推荐的安全默认值。只有明确需要在可信网络中连接未启用 TLS 的公网端点时，才应设为 `false`。回环地址和私网或局域网地址（如 `192.168.x.x`、`127.0.0.1`）始终可以使用明文连接，云元数据地址段则始终被拦截。系统会在连接时根据域名解析出的 IP 判断，因此解析到公网地址的域名也无法绕过限制。可在**出站连接**设置页面切换。

> 当 Manager 明确拒绝本机注册，例如设备数量已达上限或本机缺少设备身份时，desk-server 会暂停自动重连。**出站连接**设置页面会显示横幅说明原因，并提供**重试注册**按钮。请先从任一控制端释放一个设备名额，再重试。
- `local_signaling_token`——自动生成并保存的令牌，供本地 desk-server 及其他被控端连接同机信令服务器时认证身份。请勿手动设置；该值属于敏感凭据，日志中会自动脱敏。

### 遥测授权

`telemetry_consent` 由独立的**遥测**卡片拥有，不属于通用系统设置表单。修改端口、监听
地址、IPv6 或自启动都不能改变或清空该值。授权选择会立即持久化，但 OpenTelemetry
遥测导出组件在进程启动时创建；必须重启相关的服务端、守护进程和工作进程后，才能
把新选择描述为已经在运行时生效。

### 本机远程访问展示

<code>host_access_indicator_enabled</code> 控制 Tauri 被控端是否显示持续远程访问状态卡、托盘活动徽标和首次会话通知，默认 <code>true</code>。关闭只会隐藏这些提示，不会改变审批、权限或已经建立的会话；该本机偏好不能由 Manager 远程修改。见[被控端远程访问状态指示器](/zh/features/host-access-indicator)。

## 日志 `[log]`

- `log_level`——日志级别（`error`、`warn`、`info`、`debug`、`trace`）。
- `traceback`——是否启用 Rust 错误回溯。
- `log_retention_days`——日志保留天数（默认 `7`）。设 `0` 关闭清理。
- `log_cleanup_threshold_percent`——触发清理的磁盘占用阈值（默认 `90`）。磁盘占用超过该值时，会从最旧开始继续删除保留期内的滚动文件，直到占用回落到阈值以下；当天与前一天的文件始终保留，因为它们可能仍被写入方持有。设 `0` 关闭这一档，只按保留天数清理。
- `log_cleanup_interval_hours`——清理任务的间隔小时数（默认 `12`）。设 `0` 关闭清理。
- `tokio_console_enabled`——启用 tokio-console 订阅器（需 `tokio_unstable` 构建标志，默认 `false`）。

清理覆盖日志目录下**所有组件**的滚动文件（`desk-server.log.<日期>`、
`desk-daemon.log.<日期>`、`desk-worker.log.<日期>`、`desk-mcp.log.<日期>`、
`desk-tauri.log.<日期>`）；同目录下属于其他程序的文件不会被碰。**每个文件都带日期
后缀**，包括当前正在写入的那个——保留期从它的日期算起，所以活跃文件不会被删除。
滚动日期按 UTC 计。

日志永不阻塞主链路：stdout 与日志文件都经独立写入线程 + 有界有损队列落盘。出口停止
消费时（例如磁盘写满、写入线程卡住，或控制台在锁屏后停止响应），系统会**丢弃记录**，而不是把压力传回
调用方，采集、输入注入与信令照常运行。

清理只在拥有日志目录的进程里执行：守护进程，或便携 / 纯信令模式下的服务端。会话
工作进程与 MCP stdio 进程从不清理——它们生命周期短，且持有的是启动时的设置快照，
上面这些参数的改动不会传达给它们。

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

**TURN 设置**页会以“运行状态”卡片呈现同样的信息：服务是否在运行、正在服务的是哪些接口、未运行时的原因，以及被拒绝的条目。保存之后请看这张卡片——表单显示的是配置，卡片显示的是这份配置最终的结果。

保存会在端口绑定完成之前返回，因此刚保存完卡片显示的是“启动中”而非“运行中”——这是正常路径而非失败，只有确实有原因可报的状态才会被称为失败。主机尚未稳定时卡片会自行重读，所以它会自动转为运行中、或指出出了什么问题，不需要手动刷新。

页面还提供折叠的**高级统计查询**：输入已知客户端 `IP:端口` 与接口后，可查看该地址
的 relay/control 收发字节与包数，并区分“TURN 未运行”和“地址无记录”。当前 TURN
实现无法枚举全部会话或强制关闭单会话，因此原来的 `GET /api/turn/session` 与
`DELETE /api/turn/session` 空壳不再属于 API，只保留 `/api/turn/session/statistics`。

## 桌面 `[desk]` {#desktop-desk}

- `video_fps`——视频帧率（默认 `60`）。降低可减少 CPU 与带宽占用。
- `video_quality`——视频编码质量（`0`–`63`，越低越好，默认 `22`）。
- `video_encoder` / `audio_encoder`——可选；省略时自动选择。视频可为 `X264` / `VP8` / `VP9` / `H264` / `AV1`；音频为 `Opus`。
- `video_device_name`——要采集的显示器 GDI 设备名（`\\.\DISPLAYn`）；留空表示“首次连接时让浏览器选择”。
- `show_mouse`——是否采集并显示鼠标光标。
- `enable_dirty_rect`——是否启用脏矩形增量编码。
- `[desk.private_screen]`——防窥屏设置（`enabled` 等）。

## 虚拟显示器 `[virtual_display]` {#virtual-display-virtual-display}

- `enabled`——是否启用虚拟显示器（需已安装 IddCx 驱动；仅在特定模式下生效）。
- `exclusive` / `prompt_ms` / `adaptive_*`——独占模式与自适应分辨率参数。

## 安全 `[security]`

对入站远程会话的按能力访问控制。每项能力为三态：未设置表示“每次询问本地用户”（文件默认），`true` 表示“始终允许”，`false` 表示“始终拒绝”。

初始化向导会在安装时写入一组明确的安全设置，为设备所有者开放相应能力，因此通过向导安装的被控端不会从配置文件的“全部询问”默认值开始。对于通过[访问码](/zh/guide/access-codes)建立的非所有者会话，系统会综合这些全局设置、访问码的能力上限和现场审批结果，并采用其中最严格的限制。

- `allow_remote_control`——鼠标 / 键盘输入。
- `allow_clipboard_sync`——剪贴板同步。
- `allow_system_audio_capture`——采集并传输被控端的系统音频。
- `allow_private_screen`——防窥（隐私）屏模式。
- `allow_whiteboard`——白板叠加。
- `allow_terminal`——远程终端访问。
- `allow_file_browse`——列目录和查看文件元数据。
- `allow_file_delete`——文件删除；执行删除时还必须同时允许 `allow_file_browse`。
- `allow_file_transfer`——文件上传 / 下载。
- `approval_timeout`——授权提示框的等待时长（秒）。**默认 `30`**——超时后由被控端**服务端权威地取消（拒绝）该请求**，而不是让它永久挂起；该拒绝在服务端强制执行，即使授权界面已关闭或不可达也会兜底拒绝。设为 `0` 表示永不超时（提示框无限等待）。“从不”以数值 `0` 存储，因此重启后仍然保持。

### 变更何时生效

能力设置的变更对**正在进行的会话**同样生效，无需断开控制端或重启被控端。新设置从该连接**下一次**请求该能力时起开始约束它。

但它**不会**收回控制端已经取得的能力。在有人正在控制桌面时把 `allow_remote_control` 改成 `false`，会挡住下一次控制请求，但不会结束这一次已获授权的控制；正在传输中的文件同理。要立即切断会话，请断开该连接，或使用[远程访问锁定](/zh/features/remote-access-lock)（它会一次性取消全部远程活动）。

若用户在设置正被修改的同时勾选了“记住我的选择”，该选择会被丢弃而不是被应用——你刚做的修改依然有效，而用户答复的那一次请求本身仍然按其答复处理。

## AI 设置

AI 模型服务、基础 URL、模型名称和 API 密钥通过**管理控制台**配置，而不是写入 TOML 文件。API 密钥只保存在服务端。见 [AI 诊断](/zh/features/ai-diagnostics)。

## 推荐的开发配置

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # 开发期间降低 FPS 以节省资源
```
