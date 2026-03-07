# 归档：修复 TURN 凭据泄露风险

## 概述
本任务解决了远程桌面 `TURN` 功能在信令交互过程中直接传输用户明文密码的安全风险。通过引入 `TURN REST API` 机制，为前端生成具有时效性的临时凭据。同时确保 `static_auth_secret` 自动生成并可通过前端重置。

---

## 实施方案 (Implementation Plan)

### 用户审核必读
> [!IMPORTANT]
> 1. 本方案要求在 `TURN` 设置中配置 `static_auth_secret`。如果未配置，后端将在启动时自动生成一个 32 位的随机密钥。
> 2. 更改 `static_auth_secret` 后，现有的正在进行的 TURN 连接可能会在失效后无法重连，直到重新发起信令。
> 3. 需要在 `server` 模块中引入 `hmac` 和 `sha1` 依赖以生成签名。

### 提议的变更

#### [server]
- **Cargo.toml**: 添加 `hmac` (0.12) 和 `sha1` (0.10) 依赖。
- **turn.rs (model)**: 在 `TurnObserver::get_password` 中，复刻 `turn-server` 原生的 `static_auth_secret` 解析逻辑，动态返回 HMAC-SHA1 签名作为预期密码。
- **settings.rs (model)**: 在 `Settings::new` 中增加逻辑：若 `turn.static_auth_secret` 为空，则自动生成一个随机字符串并保存。
- **settings.rs (controller)**: 添加 `/api/desk/settings/turn/regenerate-secret` POST 接口，用于重置 `static_auth_secret`。
- **signaling.rs**: 实现辅助函数 `generate_turn_credentials`。在 `init_ptc_peer_connection` 方法中，使用 `static_auth_secret` 生成临时用户名（`timestamp:username`）和 HMAC-SHA1 签名作为密码。

#### [vite-project]
- **system-settings.tsx**: 增加 "TURN 安全配置" 区块，添加 "重新生成 TURN 密钥" 按钮及确认对话框。
- **i18n**: 补充中英文翻译键值对。

---

## 任务执行记录 (Task List)
- [x] Analyze current TURN credential handling
- [x] Design a secure TURN credential mechanism
- [x] Implement the fix
    - [x] Modify `Cargo.toml` to add crypto dependencies
    - [x] Update `Settings::new` for automatic secret generation
    - [x] Create regenerate secret endpoint in `settings.rs`
    - [x] Update `TurnObserver::get_password` (use only secret logic)
    - [x] Implement temporary credential generation in `signaling.rs`
    - [x] Update `SystemSettings` UI with regeneration button
- [x] Verification
    - [x] Test TURN connectivity with temporary credentials
    - [x] Verify that sensitive passwords are no longer leaked

---

## 执行总结 (Walkthrough)

### 变更总结
本任务解决了远程桌面 `TURN` 功能直接传输用户明文密码的安全漏洞。通过引入 `TURN REST API` 机制，现在服务端能为前端生成 24 小时有效的临时凭据，而非暴露用户主密码。

### 验证方法
1. **自动生成验证**: 删除现有配置中的密钥，启动服务端，确认自动填充了新密钥。
2. **前端重置验证**: 在设置页面点击重置，确认弹出成功提示并要求重启。
3. **连接功能验证**: 重启后发起连接，通过控制台确认 `credential` 已变为签名字符串。
4. **兼容性验证**: 确认 `TurnObserver` 已按照 RFC 5766 规范兼容了临时凭据解析。
