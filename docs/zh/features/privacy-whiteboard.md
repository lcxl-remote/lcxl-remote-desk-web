# 防窥屏与白板

这两个功能在被控机本地渲染，因此需要 **Tauri 桌面客户端**（`tauri-app`）。

## 防窥屏

锁定本地显示与输入，确保远程操作期间的隐私——远端机器旁的旁观者既看不到屏幕，也无法干扰输入。

防窥屏设置位于 `config.toml` 的 `[desk.private_screen]` 下。

## 远程白板

直接在远端屏幕上绘制与批注以便协作——适合引导式支持与演示。

## 运行 Tauri 客户端

```bash
cd tauri-app
cargo tauri dev
```

见[快速开始 → Tauri 桌面客户端](/zh/guide/quick-start#方式二-tauri-桌面客户端)。
