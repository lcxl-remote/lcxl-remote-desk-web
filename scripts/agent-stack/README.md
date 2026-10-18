# Agent 栈预算检查

检查目标是实际生成的构造、poll、visitor 和 drop 单帧，不能用 `size_of::<Box<_>>()` 或单次业务测试替代。共享循环按准备、上下文、模型和工具阶段驱动；宿主准备阶段返回拥有状态的执行 future，上一阶段返回后再 poll 下一阶段。继续使用原有任务所有者，不增加生产任务或线程，也不扩大栈。

工具需要 Python 3.11+（CI 使用 3.12）和仓库钉死的 Rust 1.90。在 Web 根目录运行：

```bash
python3 -m unittest discover -s scripts/agent-stack -p test_report.py
python3 scripts/agent-stack/run.py --package desk-diagnose-core --output target/agent-stack/core-debug.json
python3 scripts/agent-stack/run.py --package desk-signal --output target/agent-stack/signal-debug.json
python3 scripts/agent-stack/run.py --package desk-signal --profile release --output target/agent-stack/signal-release.json
cargo test -p desk-diagnose-core -p desk-signal --lib --features desk-diagnose-core/stack-budget
cargo test -p desk-signal --lib --release command_history -- --nocapture
```

父仓根目录的 Manager 使用同一工具，无需依赖 OSS signal 实现。父仓还会用自己的 lockfile 单独检查共享核心，不能拿 Web 的依赖图实测代替宿主依赖图：

```bash
python3 web/scripts/agent-stack/run.py --workspace . --package desk-diagnose-core --output target/agent-stack/core-debug.json
python3 web/scripts/agent-stack/run.py --workspace . --package lcxl-remote-desk-manager --output target/agent-stack/manager-debug.json
cargo test -p lcxl-remote-desk-manager --lib --features desk-diagnose-core/stack-budget
LRDM_TEST_REDIS_URL=redis://127.0.0.1:6379/15 cargo test -p lcxl-remote-desk-manager --lib --features desk-diagnose-core/stack-budget automatic_ui_review_with_command_history_fits_small_stacks -- --ignored --nocapture
```

Redis 必须为隔离测试实例。Manager 的并发事务证明测试另使用 `LRDM_TEST_PG_URL` 指向隔离 PostgreSQL；不要指向产品运行库。测试网关和设备回执均为合成数据。

## 报告与基线

`check.py` 先编译一个已知超过 64 KiB 的局部数组，确认当前 rustc 支持栈报告。它只给指定 crate 注入 LLVM remark；未使用的唯一 cfg 强制该目标重新 codegen，不清空依赖缓存。解析流只保留紧凑报告。构建未成功、产物仍为 fresh、零记录、符号格式不支持、关键族缺失都会失败。编译备注不包含 prompt、会话或凭据。

`run.py` 同时施加 64 KiB 单帧上限与该平台/profile 已评审的 `stack-budgets/<package>-<target>-<profile>.json` 棘轮。未建立基线的平台仍施加硬上限、关键族检查和小栈回归，但不能称为该平台的历史棘轮已建立。首次 CI 报告需经过评审再登记；不能复制另一平台/profile 的数值。debug/release 分别登记工具链、目标、Cargo profile、特性、Rust flags 和影响编译的配置文件；Web 与 Manager 依赖上下文严格分开。报告另记录 lockfile 指纹供审查，依赖更新仍重测并遵守原逐帧棘轮，不能借升级重置预算。

`--report-only` 仅采集证据，不是门禁通过。本机执行时自动和当前 HEAD 比较基线；CI 通过 `AGENT_STACK_BASE_REF` 指定 PR base / push-before revision，完整历史 checkout 防止多提交推送重置预算。删除已登记基线、提高预算、移除必检族或混用构建配置都会失败。更新基线须检查实际下降，也可用 `check.py --previous-baseline <旧基线>` 独立验证。存量拆出的阶段通过 `lineages` 登记唯一来源与全部成员，嵌套使用 `sum`；只有代码和回归证明上一 poll 已返回的顺序阶段才能使用 `max`，驱动帧仍相加。来源不能删除、重复分配，母项不能保留旧预算后另添子项。本轮 macOS debug 对最初两个大执行族登记保守相加账本；所有新增阶段均已满足 64 KiB，不需要继承中间里程碑的大帧豁免。release 和 Manager 依赖图下的共享核心按各自本轮实测建立首次逐帧棘轮，不借用 debug 或 Web 依赖图的来源额度；来源关联记录在 `derivations`。账本不是完整调用图，也不表示完整峰值。

## 小栈与原始 future

压力测试使用独立测试子进程，在显式 1 MiB 和 2 MiB 线程上运行真实编排及审批链，启动时移除继承的 `RUST_MIN_STACK`。检查点核实实际执行线程；沿用生产已有任务边界，避免大型 fixture 叠加，也不能把产品 poll 转移到未限定线程来通过。原代码在 macOS ARM64 的 2 MiB 组合上有失败反证；其他平台不要求原代码稳定溢出。

`stack-budget` 是开发检查特性，启用后在类型擦除前断言阶段原始 future 不超过 16 KiB。类型布局、future 状态、单帧和完整组合是四项不同指标。跨线程后台工作继续使用保持 `Send` 的阶段，不以局部执行 future 破坏现有 `tokio::spawn` 契约。取消/drop 必须仍释放原 guard、租约和转发器。

完整会话 decode 保持事务内的完整验证。同事务复用证明只省 serde：scope、根行和 budget 仍强制 SELECT，核对原主体、version/lease、行身份与原始 JSON。PostgreSQL READ COMMITTED 下连续查询可能读取不同已提交版本；不能把同一事务等同于同一快照。证明仅持有实际事务引用，不跨会话写入、事务或请求缓存。
