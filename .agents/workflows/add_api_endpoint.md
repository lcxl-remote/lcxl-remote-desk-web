---
description: 引导式添加新 REST API 接口
---

当需要新增后端 API 接口及其前端调用时，执行此工作流。

// turbo
1. **后端开发 (Controller & Service)**
   - 在 `server/src/model/` 中定义 Request/Response 结构体（确保派生了 `ToSchema`）。
   - 在 `server/src/service/` 中实现业务逻辑。
   - 在 `server/src/controller/` 中编写路由处理器，并在 `main.rs` 中注册。

2. **接口描述**
   - 在路由处理器上添加 `#[utoipa::path(...)]` 注解。

3. **同步前端**
   - 执行工作流 `/update_openapi` 自动生成前端 Hook 和类型定义。

4. **前端调用**
   - 寻找合适的 React 组件或页面集成。
   - 使用 Kubb 生成的 `use...` React Query 钩子进行接口调用。
