# 归档：修复前端构建中的 Client 类型错误 (2026-03-17)

## 任务背景
前端项目在构建阶段（`npm run build`）报错，原因是 Kubb 生成的服务代码对 Axios 客户端的泛型调用顺序与 Axios 原生签名不一致，导致 `tsc` 类型检查失败。

## 任务列表
- [x] 调研构建失败的具体原因
    - [x] 运行构建脚本获取错误日志
    - [x] 分析错误根源
- [x] 修复发现的问题
    - [x] 修改 `src/lib/kubb-client.ts` 中的 `Client` 类型和导出
- [x] 验证构建是否成功

## 实现计划
由于 Kubb 生成的代码中，对 `client` (axios 实例) 的泛型调用方式与 Axios 原生的泛型签名不匹配，导致 TypeScript 报错。在 `src/lib/kubb-client.ts` 中重新定义 `Client` 类型和 `client` 包装函数，以符合 Kubb 的预期。

### 拟议变更
- 导出 `axiosInstance` 以供内部使用。
- 重新定义 `Client` 类型，使其接受三个泛型参数：`TData`, `TError`, `TVariables`。
- 将默认导出的 `client` 包装，显式映射泛型到 Axios 的 `request` 方法。

## 执行总结
- **修改文件**：`vite-project/src/lib/kubb-client.ts`
- **修复方案**：实现了一个符合 Kubb 期望签名 `<TData, TError, TVariables>` 的包装器，内部代理到 Axios 的 `.request()`。
- **验证结果**：在 `vite-project` 目录下运行 `npm run build` 成功通过，输出了 `dist` 产物。

---
*归档由 Antigravity 自动生成。*
