# 主动断开与远程访问安全锁定

被控端状态卡提供两个语义不同的本机安全动作：

- **断开此会话**会结束选中连接的桌面、控制、终端、文件管理、传输和执行活动，但不会阻止对方再次连接。
- **全部断开并锁定远程访问**会先关闭被控端准入栅栏，再结束所有当前活动，并让锁定跨 daemon、worker 和界面重启保持。只有本机用户解锁后，owner、组织、设备码、支持码和 grant 才能再次访问。

锁定是应急停止动作，不是首次启动策略。新安装和缺少状态文件时默认 unlocked；已经存在但不可读或无效的状态文件会显示为 **Recovery locked（恢复锁定）**，并按 locked 拒绝访问。
恢复锁定会先与已配置的中央服务收敛出权威锁轮次，不能在丢失 `lock_id` 后靠猜测直接解锁。

## 本机身份认证

HTTP、signaling、manager 设置、MCP 和远程终端请求都不能解锁。Tauri shell 会打开操作系统原生提权认证：Windows 使用 UAC，Linux 通过 `pkexec` 使用 polkit/PAM，macOS 使用管理员认证对话框。认证后的已安装 helper 再通过本机命名管道或 Unix socket 获取并消费一个短时、绑定动作与状态版本的 challenge。取消认证、错误可执行文件、过期版本、过期 challenge 或重放都会保持 locked。

无界面主机使用同一条本机安全通道：

```text
lcxl-remote-desk-server access status
lcxl-remote-desk-server access lock
lcxl-remote-desk-server access unlock
lcxl-remote-desk-server access disconnect <connection-id>
```

`lock` 必须在本机交互式终端中运行；`unlock` 必须从已提升权限的终端运行（“以管理员身份运行”、`sudo` 或平台等价方式）。daemon 离线时，`status` 只能只读查看持久状态；其余命令都要求 daemon 正在运行。

## 持久性与中央纵深防御

daemon 将本机安全事实与普通设置分开保存，并使用 durable atomic replace。本机栅栏始终是最终权威。如果写入失败，当前会话仍会停止且当前进程保持 locked，但状态卡会明确提示锁定只存在于内存、重启后可能丢失；重启前应先重试。

配置的中央服务可达时，被控端还会同步一份锁镜像。中央服务拒绝新准入、每个锁定轮次只推进一次授权代际，并尝试关闭活动 browser peer。中央离线不影响本机锁定成功，重连后会继续补偿同步。本机 OS 认证是解锁的充分条件：daemon 不等待任何 central 响应，直接持久化并应用本机 unlocked；central 同步只是持久后台 outbox。若异常恢复后的本机状态不知道 central 已有的锁 fence，daemon 会从 ack 学习该 fence 并重试中央解锁，但不会再次关闭本机 gate。解锁不会恢复旧 grant、支持码、审批或会话。持久设备码字符串不会被轮换，但锁定前签发的 grant 仍永久失效。

## 凭据补救

锁定只保证这台被控端停止接受远程工作，并不能证明账号密码、浏览器会话、API 令牌或已经复制的数据安全。请根据疑似泄露来源轮换或撤销相应凭据。退出 Tauri 界面不会解除 daemon 锁定；远程访问正在进行或已锁定时，原生退出确认会明确说明这一点。
