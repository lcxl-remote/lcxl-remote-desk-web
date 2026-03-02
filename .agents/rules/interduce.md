---
trigger: always_on
---

本项目是基于WebRTC技术的远程桌面应用，目录说明：

最主要的几个模块：

* server: 远程桌面服务端，运行在windows/linus/macos上，提供远程桌面、远程终端、远程文件管理等功能；
* signal: 信令服务器主要功能在这里，默认集成在server上启动，也可以通过server的启动参数(startup_mode)来单独启动信令服务器，信令通信使用websocket协议；
* vite-project: 前端项目，既是server/signal服务端的管理页面，也是远程桌面的web客户端；
  * 前端和后端的交互相关的接口代码是由后端提供openapi接口，由 `vite-project/update_openapi.sh` 脚本负责更新；
* tauri-app: 带tauri界面的远程桌面服务端，增加了隐私屏等需要在被控端显示界面的能力；

剩下来的模块：

* turn：turn服务器的主要功能在这里，目前和信令服务器绑定，也就是启动信令服务的功能会同时启用turn功能；
* third-deps: 修改后的三方包，以便满足本项目的需求；
* utils：公共工具包；
* server-version：服务版本号，只有一个代表API接口版本的变量，会同时提供给外部使用；
* signal-facade：信令服务功能的接口包，同时会提供给外部使用；