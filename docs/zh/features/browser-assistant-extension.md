# 浏览器助手扩展

LCXL Chrome 扩展是AI助手默认的浏览器适配器。它运行在被控端 Chrome 中，通过带认证的 loopback bridge 与被控端连接。Chrome DevTools MCP 仅作为默认关闭的开发调试适配器保留，系统不会自动回退到它。

## 一次配对

1. 在被控端 Chrome 打开 `chrome://extensions`，启用“开发者模式”，选择“加载已解压的扩展程序”，然后选择仓库中的 `browser-extension` 目录。
2. 在本机 OSS AI助手页面点击“显示配对码”。该接口仅允许设备 owner 访问，并返回 `no-store`。
3. 打开扩展弹窗，填入 bridge 地址和配对码，再点击“配对这个浏览器”。认证连接成功后，弹窗会显示已连接。
4. Gmail 与 Slack 已内置。操作其他 HTTPS 站点前，先打开该站点，再在扩展弹窗点击“允许当前站点”；这次站点授权由 Chrome 自己提示。

配对信息保存在当前 Chrome profile 中。之后的 typed 浏览器操作不再要求 DevTools 远程调试确认。切换 Chrome profile、清除扩展存储或轮换被控端数据都会使原配对失效。

## 安全边界

扩展只接受AI助手公开的版本化 typed action：打开或导航页面、获取有界 accessibility 快照、等待 opaque element、填写审核过的字段、上传经过精确验证的工件字节，以及在对应 grant 下激活 opaque element。它不开放任意 JavaScript、原始 DOM、Cookie、Storage、历史记录、网络日志、下载管理或本机文件路径。

密码不会进入投影。附件在跨越 edge bridge 前会核对大小与 SHA-256，扩展内部还会再次核对。页面和元素引用绑定 Chrome profile、标签页、文档 incarnation、origin 与 revision；页面导航或扩展重连后，旧引用会 fail closed。

Gmail 与 Slack 的草稿准备不会激活“发送”。未来的 exact-send 必须绑定单独封存的 `SendExternal` payload，在其独立发布门通过前保持不可用。
