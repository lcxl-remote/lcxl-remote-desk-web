# 虚拟显示器

LCXL Remote Desk 可向被控设备呈现一个**虚拟显示器**，适用于无头主机，或为远程会话专门分配一块屏幕。

::: tip 平台支持
虚拟显示器基于 Windows Indirect Display Driver（**IddCx**），需要已安装驱动。它仅在特定启动模式下生效；其他模式会拒绝相关信令。
:::

## 配置

虚拟显示器由 `config.toml` 的 `[virtual_display]` 控制：

- `enabled`——开启虚拟显示器（需已安装 IddCx 驱动）。
- `exclusive`——独占模式开关。
- `prompt_ms`——切换前的倒计时提示时长。
- `adaptive_*`——自适应分辨率参数。

完整字段列表见 [config.toml 参考](/zh/config/config-toml#virtual-display-virtual-display)。

## 用户态抽象

用户态部分实现为一个 Rust crate（`desk-virtual-display`），含一个 trait 加 Windows IDD 实现及其他平台的 stub。驱动安装/卸载由 `desk-virtual-display-driver-ops` 封装。
