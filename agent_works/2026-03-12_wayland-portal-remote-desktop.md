# 2026-03-12 Wayland Portal 远程桌面链路完善

## 1. 实现计划 (Implementation Plan)

### 目标描述

在 Wayland 环境下实现稳定的远程桌面屏幕共享与控制链路，解决授权弹窗、阻塞等待、初始化时机不一致、日志噪音等问题，并让前端有明确的初始化/授权提示。

### 拟议变更

#### 后端服务端 (server)

- **[MODIFY] Signaling 初始化流程**: 在初始化阶段异步获取图像捕获列表，触发 Wayland portal 授权流程。
- **[NEW] Wayland portal capture & RemoteDesktop**: 引入 portal client、Wayland capture 和远控控制通道实现。
- **[MODIFY] PipeWire/Portal 流程健壮性**:
  - 修复 DBus 信号竞态（先订阅再调用）
  - 处理 DBus 响应签名不匹配
  - 避免日志输出原始帧数据
- **[MODIFY] 捕获初始化逻辑**: 取消阻塞等待首帧格式，避免前端退出导致后端卡死。
- **[MODIFY] 诊断接口**: 增加后端运行态诊断信息用于前端展示。

#### 前端项目 (vite-project)

- **[MODIFY] 远程桌面初始化 UX**: 增加“初始化/等待授权”占位提示。
- **[MODIFY] Desk 配置**: 移除 Linux backend 选择，保留 Wayland 控制模式。
- **[MODIFY] i18n**: 增加初始化提示文案的中英文翻译。
- **[MODIFY] OpenAPI 生成**: 同步新字段与类型。

---

## 2. 任务清单 (Task List)

- [x] 增加 Wayland portal 捕获与远控通道实现
- [x] 修复 portal DBus 信号竞态与响应签名问题
- [x] 过滤帧日志输出，降低刷屏风险
- [x] 捕获初始化改为非阻塞流程
- [x] 初始化阶段异步枚举捕获设备，触发授权
- [x] 前端增加初始化/授权提示与配置调整
- [x] i18n 文案同步
- [x] OpenAPI 同步生成
- [x] 编译与构建验证

---

## 3. 实现成果 (Walkthrough)

### 关键特性

1. **Wayland portal 捕获与控制链路完善**  
   屏幕共享采用 `org.freedesktop.portal.ScreenCast`，控制通道采用 `org.freedesktop.portal.RemoteDesktop`，并引入共享会话逻辑用于输入控制。

2. **授权触发时机前移**  
   初始化阶段异步获取捕获列表，即可触发系统授权对话框；拒绝授权的模式会被过滤，不再出现在 UI 选择项里。

3. **DBus 响应可靠性提升**  
   修复了信号订阅与调用顺序的竞态；修复了 SelectSources/SelectDevices 的响应签名解析错误。

4. **日志噪音控制**  
   将帧数据日志由原始 `Vec<u8>` 改为尺寸/字节数摘要，避免刷屏。

5. **初始化等待不卡死**  
   PipeWire 捕获初始化不再阻塞等待首帧格式，前端退出时不会让后台卡死。

6. **前端初始化提示**  
   在远程桌面占位区域增加“初始化/等待授权”提示，提高可理解性。

### 验证结论

- `cargo check -p desk-signal-facade -p lcxl-remote-desk-server` 通过
- `npm run build` 通过
- OpenAPI 同步完成（生成文件已更新）

---

## 4. 后续建议

- 如果要规避“初始化授权 + 实际录屏二次弹窗”，建议复用 portal 会话/FD 并在初始化时缓存（待评审后再实施）。
- 可考虑加入“等待授权超时提示/取消”的 UI 状态，避免用户误以为卡死。
