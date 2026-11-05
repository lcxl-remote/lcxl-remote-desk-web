# config.toml 参考

服务端设置使用与启动模式无关的平台默认路径：Windows 为 `%ProgramData%\LCXL Remote Desktop\config\config.toml`；Linux 普通用户为 `XDG_CONFIG_HOME/lcxl-remote-desk/config.toml`（未设置时 `~/.config/lcxl-remote-desk/config.toml`），root 为 `/etc/lcxl-remote-desk/config.toml`；macOS 为 `~/Library/Application Support/com.lcxl.remote-desk/config/config.toml`。`-c, --config-file-path <PATH>` 用于显式选择其他 profile；只有显式传值时服务或 LaunchAgent 才继承该路径。

系统、连接、日志、TURN 服务、TURN 客户端及 AI 执行与采集策略保存后，页面会重新读取已保存配置并更新缓存，返回页面时显示已保存的值。保存按钮在刷新完成前保持禁用，刷新期间继续编辑的内容会保留。如果写入成功而读取失败，提示会明确说明配置已保存，并请你重新加载页面确认已保存的值。

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
实现无法枚举全部会话或强制关闭单会话，因此 API 只提供
`/api/turn/session/statistics` 这一个按地址查询的端点。

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

## OSS 全局配置

OSS 的 AI 网关、审批网关、搜索和策略配置与主机设置保存在同一份 `config.toml`，路径由当前 profile 或 `--config-file-path` 决定。设置页面保存后立即写入这份文件；停服后也可手工编辑，重新启动加载。下列配置段及 `[oss_config_metadata]` 不接受 `LRD_*` 环境变量覆盖；其他主机设置保持原有环境变量行为。

| 配置段 | 字段与含义 |
| --- | --- |
| `[device_codes]` | `length`：新设备码生成长度，默认 `8`，范围 `6`～`64`；只影响首次分配和主动重新生成，保留现有码 |
| `[access_code_protection]` | 访问码请求频率与失败退避设置；也可在设备码设置页修改 |
| `[ai_gateway]` | `wire_protocol`、`base_url`、`model`、`api_key`、`supports_image_input`；`profile_schema_version`、`reasoning_contract`、`anthropic_prefix_binding`、`request_options_json`、`output_limit_field`、`runtime_max_output_tokens`、`max_context_bytes`；`response_format`、`execution_mode`、`max_steps_per_turn`、`max_same_tool_calls_per_turn`、`exec_approval_timeout_secs`（秒） |
| `[approval_gateway]` | `enabled`；独立模型配置位于 `[approval_gateway.gateway]`，字段沿用主网关。OSS 不包含价格、货币或记账配置 |
| `[web_search]` | `schema_version`、`provider`（`duck_duck_go` / `brave` / `tavily`）、可选 `api_key` |
| `[context_management]` | `schema_version`、`strategy`（`window` / `checkpoint_summary`） |
| `[subagent_policy.limits]` | `maxUnfinishedPerRoot` 为每个主会话未结束子任务数上限。 `maxTaskDurationMinutes` 为新任务的最长时长，默认 120 分钟，接受正整数 |
| `[goal_budget_policy]` | `schemaVersion`、`deviceUnavailableMaxMs`（毫秒，不可禁用）；`[goal_budget_policy.limits]` 包含 `activeTimeMs`、`deadlineMs`（毫秒）、`modelTokens`、`modelCalls`、`toolCalls`、`slices`、`stalledSlices` |
| `[schedule_budget_policy.maximum]` | `max_runs_per_utc_day`、`max_calls_per_run`、`max_model_tokens_per_run`、`max_runtime_seconds`（秒） |
| `[usage_retention]` | `turn_days`、`agent_session_days`（天） |
| `[model_metrics]` | `enabled`、`detail_days`、`five_minute_days`、`hourly_days`、`mutable_days`（天）、`detail_row_budget`、`event_row_budget`、`compact_row_budget`、`rollup_row_budget`、`series_per_bucket`、`storage_budget_bytes`（字节，十进制字符串） |

AI 网关与审批网关的测试均使用各自的 `runtime_max_output_tokens`，与实际调用共用输出上限，不再配置独立的探针输出 token。

缺段或省略字段使用现有业务默认值；上述段内未知字段和非法值会拒绝加载。配置版本字段 `revision`、`connection_revision`、`profile_revision`、`configuration_revision` 均使用十进制字符串，例如 `revision = "1"`。版本及 `[oss_config_metadata]` 的实例身份、指纹和版本记录由服务维护，无需手工修改。停服编辑业务字段时，启动过程按实际变化更新对应版本；空白和键顺序变化不会更新版本。两套网关的探测记录仍存 SQLite，手工修改网关后需按页面提示重新验证。

`request_options_json` 是 JSON 对象字符串，可以保存嵌套 `null`。长期目标的每项可选限额可写正整数或字符串 `"disabled"`，省略表示默认限额；设备不可用等待上限始终为合法正整数。

```toml
[ai_gateway]
wire_protocol = "anthropic_messages"
base_url = "https://model.example/v1"
model = "example-model"
api_key = "replace-with-your-key"
max_context_bytes = 131072
request_options_json = '{"thinking":{"type":"adaptive","display":null}}'

[approval_gateway]
enabled = false

[goal_budget_policy.limits]
modelTokens = "disabled"

[usage_retention]
turn_days = 30
agent_session_days = 30
```

API 密钥保存在这份文件中，公开设置响应继续只返回“是否已设置”。现有整文件保存及 worker 初始化保留全部配置段。上下文策略继续在模型 seam 首次使用时固定；长期目标在原有检查点应用最新预算，既有计数不重置；保留期限在下一清理周期生效。

`desk_signal.db` 只保存会话、任务、授权、探测、用量等运行数据，`model-metrics.sqlite` 只保存指标记录和运行水位。开发阶段遇到不兼容数据库结构时，停服并手工重建运行数据库；新格式不提供旧配置表的读取、转换或迁移，已保存在 `config.toml` 的配置会保留。


## 推荐的开发配置

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # 开发期间降低 FPS 以节省资源
```

## 用途输出额度

网页“上下文管理”和“终端补全”保存到以下全局段；revision 由设置提交机制维护。正整数范围为 1～4294967295，实际输出取用途额度与模型运行时额度的较小值。

```toml
[context_management]
schema_version = 2
revision = "0"
strategy = "checkpoint_summary"
summary_max_output_tokens = 16384

[terminal_completion]
revision = "0"
max_output_tokens = 512
```

### 思考请求配置

当前 profile schema 为 `2`。`reasoning_contract` 显式选择 `conservative`、`openai_chat`、`deepseek_chat` 或 `anthropic_messages`，并与 `wire_protocol` 匹配。`anthropic_prefix_binding` 默认 `false`，只在端点将签名思考绑定到请求前缀时启用。思考强度和可选原生清理保存在 `request_options_json`；详见 [AI 诊断](../features/ai-diagnostics.md)。主模型与审批模型使用相同字段。

AI 助手资料策略文件与管理 API 使用相同字段规则。无效值会指出字段；启动与成功保存策略时仅记录实际命中的提示。提示语义及共享读取预算见[AI 助手记忆与历史策略](#assistant-data-policy)。

## AI 助手记忆与历史策略 {#assistant-data-policy}

平台通过 `AssistantDataPolicy.limits` 配置记忆内容与检索工作量。登录当前开源 Signal 的管理账号后，`GET /api/admin/system/ai-assistant-data` 返回 `data.policy` 与 `data.issues`。修改完整策略后，以 `expected_revision` 和 `policy` 提交 `POST`，保留读取时的 `policy.revision`；成功后返回实际保存的策略及本次命中的问题。版本冲突时重新读取。配置原子写回实际使用的 `config.toml`。

正常配置的 `issues` 为空。每个问题包含 `level`（`error` 或 `warning`）、稳定 `code`、相关配置字段 `fields`、兜底说明 `message`，以及适用时的 `effective_limit`。硬错误使用 HTTP 200、`success=false` 和 `INVALID_PARAMS`，保存不生效；仅有提示时 `success=true`，保存照常完成。版本冲突继续使用 `REVISION_CONFLICT` 与 `current_revision`。

提示仅覆盖能从配置直接确定的限制：相关性结果条数超过候选数、历史候选数超过会话数乘每会话片段数、模型状态摘要数超过保留项目数、本人历史扫描数超过总扫描预算，以及当前记忆选取数超过存储或扫描上界。正文、索引、查询状态与元数据的实际占用取决于内容，不根据估算返回容量不足提示。OSS 文件加载遇到硬错误会指出字段；启动与成功保存策略时仅记录实际命中的提示。

本轮读取剩余额度为零时停止新的读取；正数额度按单次响应上限与剩余总量的较小值尝试。是否装得下必要元数据和完整来源，由实际 JSON 与工具消息封装容量检查决定。调用次数、实际结算及原调用重放继续受原预算约束。历史、记忆及旧版本的数量、正文、索引与元数据配额同时生效；旧版本数量达到上限时，更新仍会返回容量不足，直至版本清理释放容量。

聊天、子助手和手动定时任务的正文按原文提交；正文容量、记忆总数及资料结果的业务额度由服务端统一校验。三端按服务端结果展示记忆、历史、引用和处理状态，保留类型、身份、版本、来源对应、去重与游标一致性校验。个性化与关键词的界面预检读取当前服务端策略，普通列表仍按界面选定的数量分页。服务端拒绝提交时显示错误并保留可编辑草稿；发送中的聊天输入在确认接纳后清空。


普通聊天输入、引用和状态展示也由同一平台策略配置，路径相对于 `policy.limits`：

| 配置路径 | 默认值 | 用途 |
|---|---|---|
| `conversation.question_bytes` | 16 KiB | 普通聊天正文的 UTF-8 字节准入 |
| `conversation.page_bytes` | 256 KiB | 本人历史搜索、留存会话列表、来源窗口及聊天记录的统一分页目标 |
| `conversation.citations` / `citation_bytes` / `citation_page_answers` | 64 / 8 KiB / 100 | 每回答引用数量、引用编码容量及引用读取批次 |
| `retrieval.snippet_bytes` | 512 字节 | 搜索结果摘要，按完整 UTF-8 字符截取 |
| `retrieval.episode.query_devices` / `query_cursors` | 10,000 / 32 | 冻结查询的设备范围与续页游标 |
| `retrieval.episode.query_state_bytes` / `query_seen_sources` / `query_delivered_calls` | 1 MiB / 500 / 32 | 查询状态编码、来源去重与已交付调用账本 |
| `processing.status_coverage` | 50 | 历史覆盖记录展示 |
| `processing.native_status_rows` / `native_status_items` / `native_status_model_items` / `native_status_source_refs` | 25 / 24 / 3 / 4 | 每类原生状态读取数、快照项数、模型摘要项数及摘要来源数 |

查询状态沿原检索预算冻结容量；重试与翻页保持原值。调低新回答的引用容量保留已有引用，读取批次决定分批加载的工作量。内部来源账本、读取投影、导出组和事实提交沿原请求预算、账号记录总量及实际元数据配额，不再分别施加任意集合容量；真实授权截止、来源绑定、原生回执与事务版本核验继续生效。客户端按服务端结果展示任务提案、执行预览和附件策略，完整提交定时任务正文、处理备注与目录用途说明；超限由服务端返回业务错误。

记忆总数通过平台策略顶层的 `policy.max_memory_records` 配置，默认每账号 1,000 条，个人与各设备范围共用。独立记忆记录各计一条，同一记忆的修订与幂等重放不重复计数；仍保留的待确认、被替代和遗忘记录也占用存储条数。达到上限时，新记录写入失败并回滚；更新已有记录继续按原正文及元数据配额处理。调低上限保留已有记录，调高后可继续新增。配置沿上述完整策略接口保存，该字段与 `policy.limits` 同级。

本人打开来源后的留存历史窗口每页最多 20 个完整片段，按游标加载更早或更新的内容，窗口总大小随实际内容返回。模型工具的单次回复继续使用 `retrieval.episode.response_bytes` 配置。

`conversation.page_bytes` 统一本人历史搜索、留存会话列表、留存来源窗口与普通/来源定位聊天页的分页目标。页面保留完整片段、原生工具消息组、思考和引用；单个完整条目或消息组超过目标时仍完整返回，其余内容通过分页读取。条数与权限规则沿用原接口；这不是正文准入上限。本人搜索候选使用 `retrieval.query_candidates`，游标编码复用 `retrieval.episode.query_state_bytes`，命中词项按实际查询词集合读取；来源查找使用已有文档、扫描页和时间预算。本人搜索游标继续绑定原策略 revision，策略改变后重新搜索。已经保存的记忆来源可完整读取与遗忘，降低 `memory.source_refs` 只影响新准入。

默认每组关键词允许 256 个 Unicode 字符（按标量值计数），中文和单个 emoji 码点各计一个；组合字符分别计数。关键词组数与每组字符数分别配置。五项检索扫描时限均默认 3 秒。正文、背景、依据及个性化容量使用 UTF-8 字节单位；未指定的配置沿用默认值。

`retrieval.read_messages` 配置单次读取的会话消息或记忆版本条数，默认 20；`retrieval.episode.response_bytes` 统一配置记忆搜索、历史搜索及正文读取的单次完整回复容量，默认 32 KiB（包含 JSON 和工具消息框架）。正文读取还受本轮剩余 `read_bytes` 额度约束。新轮次冻结配置；重试和续页使用原预留及已消费额度。

记忆历史版本查询还复用 `retrieval.query_candidates` 的单次扫描条数和 `retrieval.memory_keyword_scan_ms` 的扫描耗时预算。预算受限时，结果会提供后续页、标记缺口或返回预算不足。

结构化类型配置是已有规范化类型的启用列表：`response_language`、`preferred_programming_language`、`preferred_tool`、`project_directory`。其余稳定事实使用自由文本。平台整体上下文、单次工具响应、来源权限与实际存储配额继续生效。已保存内容按原身份与摘要读取，调低正文容量只约束后续新写入。

普通输入及任务创建的新检索 episode 冻结其额度，审批、恢复和翻页保留已用量。模型工具 schema、Web 和移动端输入控件读取策略中的限制。

查询快照/游标（`retrieval.episode.query_ttl_ms`）与未使用读取结果（`retrieval.episode.unused_projection_ttl_ms`）均默认 1,800,000 毫秒（30 分钟），可设为正整数。时限随原检索批次冻结，普通新输入或任务实例首次领取读取当前策略，恢复、审批及 Goal 纠正沿用原值。查询与工具结果从原预留时刻计时，自动注入的记忆块从准备时计时；更早的任务、来源和执行租约截止继续生效。翻页和重试沿用已保存的绝对截止时间；已保存到当前会话的结果随该会话保留；临时注入的当前记忆继续按本轮授权处理，原策略版本检查继续生效。

```toml
[assistant_data_policy.limits.memory]
user_instructions_bytes = 16384
source_refs = 12

[assistant_data_policy.limits.retrieval]
query_groups = 6
query_chars = 512
search_window_days = 120
history_keyword_scan_ms = 5000
owner_scan_documents = 256

[assistant_data_policy.limits.retrieval.episode]
relevance_calls = 6
time_pages = 12
read_calls = 10
read_bytes = 262144
query_ttl_ms = 1800000
unused_projection_ttl_ms = 1800000
```

### 记忆写入

对话模型提交完整正文及可选背景，平台在一次事务中保存当前记忆、版本、来源快照、检索索引及提交回执。写入受当前上下文、记忆数量、正文、索引和元数据总配额约束。容量不足时本次写入回滚，已有记忆保留。正常对话模型的输出额度与计费规则适用。

### 后台处理策略

GC 周期及处理批次通过 `policy.limits.processing` 配置。Manager 使用共享数据库中的最新策略，OSS 从实际 config.toml 读取。后台在下一次唤醒时读取配置；已保存的检索批次仍遵守原冻结策略与授权。GC 默认每 5 分钟由既有留存循环驱动，同账号的扫描领取采用相同周期；其他统计、会话及调用留存继续按一小时运行。扫描页控制单轮数据库工作量，历史捕获批次是单次提交上限。原租约、调用身份、真实回执、策略版本及计费规则继续生效。

| 配置路径（相对于 `policy.limits`） | 默认值 | 含义 |
|---|---|---|
| `processing.output_gc_interval_ms` | 300000 | GC 唤醒与同账号重扫周期，毫秒 |
| `processing.output_gc_actor_batch_size` | 32 | 每轮 GC 账号数；OSS 范围为本机唯一账号 |
| `processing.output_gc_batch_size` | 64 | 每账号输出证明及提交回执扫描批次 |
| `processing.native_retention_page_size` | 64 | 原生回执留存扫描页 |
| `processing.history_capture_batch_size` | 64 | 历史捕获单次提交上限 |
| `processing.history_scan_page_size` | 128 | 历史时间/窗口查询的数据库扫描页 |
| `processing.current_memory_page_size` | 64 | 当前记忆选择的数据库扫描页 |

```toml
[assistant_data_policy.limits.processing]
output_gc_interval_ms = 300000
output_gc_actor_batch_size = 32
output_gc_batch_size = 64
native_retention_page_size = 64
history_capture_batch_size = 64
history_scan_page_size = 128
current_memory_page_size = 64
```
