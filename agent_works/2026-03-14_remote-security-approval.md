# 归档记录：远程桌面安全增强 - 被控端独立审批机制 (v2)

## 一、背景与目标

当前被控端对远程操作缺乏独立审批权限，导致安全防护依赖控制端自律。本次更新的目标是让被控端拥有对所有远程操作相关的独立审批权，允许用户通过 `SecuritySettings` 配置。**当配置缺失时，GUI 模式下弹出请求询问用户，无 GUI 的 Headless 模式默认拒绝。**

## 二、最终实现的变更点及任务总结

### 1. 数据模型与系统配置增强
* 新增了安全配置拦截属性模型类 `SecuritySettings`，其中包括了七大拦截能力：
  * `allow_remote_control`（远程桌面控制/键鼠拦截）
  * `allow_clipboard_sync`（剪贴板同步）
  * `allow_private_screen`（隐私屏开启）
  * `allow_whiteboard`（白板标注协同）
  * `allow_terminal`（终端命令执行）
  * `allow_file_browse`（文件列表及删除浏览能力）
  * `allow_file_transfer`（文件上传及下载传输）
* 将安全设置写入后端服务器基于 TOML 引擎的本地持久化配置中，默认置空触发询问机制。

### 2. 审批网关模块构建
* 核心服务器注入全局线程安全的 Hash 集合 `PENDING_APPROVALS` 来保存处于等待决定的请求会话标识。
* 新增 `POST /api/desk/security-settings/approval/submit` 接收前端与 GUI 用户的审批决断。
* 在远程会话的全局调度网关 `SignalingContext` 服务中添加并铺设了 `check_security_permission` 统一守卫函数。所有关键控制报文皆阻挡抛往该守卫进行权限裁决与悬挂。

### 3. Tauri 客户端交互协同
* 在 Tauri 的后台应用启动线程中，新嵌入了 `SecurityApprovalManager`。
* Tauri 会在挂起等待用户决定时，强制向前端推流派发 `CustomEvent('security-approval-request')` 调用 React UI 弹窗挂载。
* **攻克难点：**由于现代桌面系统的防止后台焦点抢夺特征，利用挂接 `window.request_user_attention` 通知托盘闪烁以及瞬间触发一次 `window.set_always_on_top` 作为防卡死体验绕过方案。

### 4. Vite React 前端开发
* 增加独立的一级后台配置路由 `System -> Security Settings` 用于让本地被控用户自行记忆审批首选项。
* 基于 `@kubb` 重新自动抽取 OpenAPI 客户端。
* **攻克难点：**为所有的 `@kubb/plugin-client` 自动生成的 Axios Hooks 全局注册拦截器解决请求抛错机制，以拦截后端特定的业务 `success: false` 为统一的异常抛出至 Toast 弹层提示，为文件管理器等组件提供一致的拒绝提示体验。
* 修复了部分组件更新状态被覆盖和缓存未及时清除的数据倒逼 Bug。

## 三、相关安全检查点

> - Tauri 端不再阻塞 Rust `std::thread` 进程以防主通信管线被占用。
> - 在锁粒度上进行了排查验证，保证没有任何方法持有 `DeskSession` 的读锁期间去调度包含修改字典的写锁导致 Deadlock 死锁。

**完成时间：** 2026-03-14
