# Clipboard Sync Feature (2026-03-04)

## 背景与需求 Overview
本项目 (lcxl-remote-desk-web) 是基于 WebRTC 的远控桌面管理平台，通过 Web 端即可对受控端设备进行管理。
本任务需实现 **剪贴板共享传输** —— 使得在远程控制期间，控制端 (浏览器) 与被控端 (Rust) 能够互相同步剪贴板内的文本及图片内容。
由于 WebRTC 数据通道 `DataChannel` 通常不适合直传过大数据流，剪贴板中的图片经过编码极有可能打满通道导致阻塞或失真，故系统要求：
1. 剪贴板文本：限制 1MB
2. 剪贴板图片：限制 25MB 以内，同时需要像文件模块一样做**分片流式传输**。
3. 如果操作由于浏览器策略被阻止（如 Safari 需要用户手势），需要在页面上给予弹窗提示，引导用户手动重试。
4. 无限回音消除：服务端与前端均需要比对 `hash` 或 `timestamp` 或通过比较写入内存数据，以避免 A 同步给 B，B 收到后触发系统剪贴板变更再度同步给 A 的死循环。

---

## 依赖与核心变更清单

### 1. 服务端 (Rust)

*   **`Cargo.toml` 变更**：
    引入 `png = "0.17"` 与 `base64 = "0.22"`，替代了原本厚重的 `image` 库，用来快速解析与构建图片。
    
*   **模型层更新 (`system_setting.rs`, `data_channel.rs`)**:
    *   为 `SystemSettingHelper` 追加了剪贴板相关的读写 API（提供 `get_text`，`get_image` 以及 `set_image_from_bytes`）。
    *   新增 `ClipboardImage` 模型表示图片格式；定义 `ClipboardEventData` 发送的数据包结构体。

*   **信令交互 (`desk-signal-facade`, `signaling.rs`)**:
    *   修改共享 `SignalingState` 给它追加了一个 `accept_clipboard_sync: bool`。
    *   在接收客户端要求控制 (RequireControl) 且验证通过后自动接受剪贴板同步许可权。
    *   在后续建立 WebRTC Peer Connection 后能够让 DataChannel 放行相关的 Event。

*   **平台特性层 (`windows`, `linux`, `mac`)**:
    给这三个操作系统的 `SystemSettingHelper` 补齐了使用 `arboard` 对剪贴板获取与设置的操作方法封装。

*   **业务逻辑层 (`clipboard_event.rs`)**:
    *   创建新的 DataChannel 通道 `clipboard_event` 进行消息订阅处理。
    *   构建消息 `handle_clipboard_event`：处理 `text`, `image_start`, `image_chunk`, `image_end` 等枚举。
    *   实现了在图片分片接收时的缓存 `ImageTransferState` 合并，以及合并完成后的 `base64::decode` 和 `png::Decoder` 再写入 `arboard`。 
    *   增加了一个每 500ms 触发的轮询机制侦测系统剪贴板更新，一旦发现并进行简单的重复值/哈希去重过滤后，推送同步给前端。
    
### 2. 前端 (React/Vite)

*   **DataChannel 装载 (`use-desk-rtc.ts`)**:
    `createDataChannel('clipboard_event', { ordered: true })` 保证它为可靠有序传输信道。

*   **核心剪贴板接管器 (`use-desk-clipboard.ts`)**:
    *   自定义了一个全新的 Hook 注入 `desk-session`。
    *   **发送监听**：使用 `document.addEventListener` 劫持并监听 `copy`/`cut` 事件；通过 `navigator.clipboard.read()` 异步读取内容。如果命中图片规则，对 Blob 转 buffer 和 Array、继而 btoa(Base64) 后分按 32KB 分段 `image_chunk` 推送。
    *   **接收监听**：对接收的 payload 判定。文本做简单的 hash 缓存（防 echo-loop）和拦截后写入本地。接收到图片 chunk 时将结果追加给 ref array 等待 `image_end`；之后合并字符串构造 `Blob` 数据写入浏览器 navigator。
    *   **降级弹窗**：对写入剪贴板发生的 `NotAllowedError` 做错误捕获处理，提供给宿主页面呈现的 `fallbackToast`。
    
*   **UI 更新 (`desk-session.tsx`)**:
    *   导入剪贴板 Hook，加入其内部封装好抛出的各类状态（启停控制、进度、错误文本等）。
    *   在主菜单导航条 `<DropdownMenu>` 旁边加入了一键切换开断剪贴板以及展示其开启状态的按钮。
    *   设计了并排放置在主频幕正上方区域的美观警告 / 读条加载状态提示 (`<Loader2>` 滚动，及 Clipboard Access Required Toast)，显著改善了传输和错误过程的用户体验。

---

## Conclusion
该特性现已完成且所有代码完全编译通过，可以正常随工程开启服务与客户端通信联调。未来可酌情改进点在于进一步研究在 WebWorker 下优化超大 Base64 的主线程停顿、探索是否支持更多的复制文件拖拽形式的共享等。
