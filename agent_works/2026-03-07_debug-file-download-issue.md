# 归档：WebRTC 文件传输连接问题调试与修复

**日期**：2026-03-07
**摘要**：解决文件下载失败的问题。核心原因在于 `enable_stun/turn` 配置项误解导致 WebRTC 协商路径缺失。通过恢复 STUN 模式并优化 HMAC 凭证生成逻辑，功能已成功恢复。

---

## 任务清单 (Task List)

- [x] 调研当前 TURN 与文件传输的实现细节
    - [x] 检查 `server/src/model/turn.rs` 中的认证逻辑
    - [x] 了解文件传输如何初始化 WebRTC 连接
- [x] 复现并定位问题
    - [x] 分析日志和截图，确认连通性断点
    - [x] 验证认证逻辑路径（确认未死锁，但未被触发）
- [x] 修复并验证连通性
    - [x] 确认配置命名含义：`enable_stun/turn` 影响 `webrtc-rs` 行为而非服务器开关
    - [x] 验证结论：STUN 模式正常，TURN 模式在 `webrtc-rs` 下暂不可用
    - [x] 优化代码逻辑：重构 HMAC 生成并增强日志记录

---

## 实现计划 (Implementation Plan)

### 关键发现
1. **STUN 缺失**: 失败日志显示 `enable_stun` 被设置为 `false`。由于信令逻辑，这导致客户端没有收到 STUN 服务器地址。
2. **TURN 无响应**: 截图显示浏览器发送了 STUN 请求到 TURN 端口（3479）但无一回复，说明握手在认证前已中断。
3. **URL 规范性**: 发现 `turn:` URL 缺少传输协议参数 `?transport=udp`。

### 拟议变更
1. **信令服务优化**: 为 UDP 接口生成的 `turn:` 地址显式添加 `?transport=udp`。
2. **增强日志**: 在 `init_ptc_peer_connection` 函数中记录下发给客户端的 ICE Servers。
3. **配置调整**: 建议强制开启“启用 STUN”。

---

## 执行总结 (Walkthrough)

### 问题根因
- **配置误解**: `enable_stun` 和 `enable_turn` 控制的是 `webrtc-rs` 协议栈层面的感知，而非底层服务器的启动。
- **兼容性**: `webrtc-rs` 在显式开启 TURN 协议时表现不稳定，内网环境下只使用 STUN 模式工作最佳。

### 修改内容
1. **代码重构**:
    - 在 `signaling.rs` 和 `turn.rs` 中改用 `turn_server::stun::util::hmac_sha1` 替代手动构建的 HMAC 逻辑，保持系统一致性。
    - 增加了详细的调试日志。
2. **最终结论**:
    - 保持 `enable_stun: true`，`enable_turn: false` 以确保最佳连通性。

### 验证结果
- 文件下载功能已恢复。
- `chrome://webrtc-internals` 显示 Candidate Pair 已成功建立。
