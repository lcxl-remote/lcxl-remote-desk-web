# Agent 栈预算检查

本次 AI 助手记忆任务按 2026-10-24 用户更新的顺序执行：先完成主链路开发，期间只做快速编译和必要的功能小测；本文全部栈门禁集中到主链路开发完成后，不在开发中反复采样。限制值和最终验收要求不变。

检查目标是实际生成的构造、poll、visitor 和 drop 单帧，不能用 `size_of::<Box<_>>()` 或单次业务测试替代。共享循环按准备、上下文、模型和工具阶段驱动；宿主准备阶段返回拥有状态的执行 future，上一阶段返回后再 poll 下一阶段。继续使用原有任务所有者，不增加生产任务或线程，也不扩大栈。

工具需要 Python 3.11+（CI 使用 3.12）和仓库钉死的 Rust 1.90。在 Web 根目录运行：

```bash
python3 -m unittest discover -s scripts/agent-stack -p test_report.py
python3 scripts/agent-stack/run.py --package desk-diagnose-core --output target/agent-stack/core-debug.json
python3 scripts/agent-stack/run.py --package desk-signal-facade --output target/agent-stack/assistant-pages-debug.json
python3 scripts/agent-stack/run.py --package desk-signal-facade --profile release --output target/agent-stack/assistant-pages-release.json
python3 scripts/agent-stack/run.py --package desk-signal --output target/agent-stack/signal-debug.json
python3 scripts/agent-stack/run.py --package desk-signal --profile release --output target/agent-stack/signal-release.json
cargo test -p desk-diagnose-core -p desk-signal --lib --features desk-diagnose-core/stack-budget -- --skip assistant_data_store::tests::retrieval_native::memory_mutation:: --skip ai_assistant_orchestrator::tests::desktop_permission:: --skip ai_assistant_orchestrator::tests::permission_object::
cargo test -p desk-signal --lib --features desk-diagnose-core/stack-budget ai_assistant_orchestrator::tests::desktop_permission:: -- --test-threads=1
cargo test -p desk-signal --lib --features desk-diagnose-core/stack-budget ai_assistant_orchestrator::tests::permission_object:: -- --test-threads=1
cargo test -p desk-signal --lib --features desk-diagnose-core/stack-budget assistant_data_store::tests::retrieval_native::memory_mutation:: -- --test-threads=1
cargo test -p desk-signal --lib --release command_history -- --nocapture
```

父仓根目录的 Manager 使用同一工具，无需依赖 OSS signal 实现。父仓还会用自己的 lockfile 单独检查共享核心，不能拿 Web 的依赖图实测代替宿主依赖图：

```bash
python3 web/scripts/agent-stack/run.py --workspace . --package desk-diagnose-core --output target/agent-stack/core-debug.json
python3 web/scripts/agent-stack/run.py --workspace . --package desk-signal-facade --output target/agent-stack/assistant-pages-debug.json
python3 web/scripts/agent-stack/run.py --workspace . --package desk-signal-facade --profile release --output target/agent-stack/assistant-pages-release.json
python3 web/scripts/agent-stack/run.py --workspace . --package lcxl-remote-desk-manager --output target/agent-stack/manager-debug.json
cargo test -p lcxl-remote-desk-manager --lib --features desk-diagnose-core/stack-budget -- --skip assistant_data_store::tests::retrieval_native::memory_mutation::
cargo test -p lcxl-remote-desk-manager --lib --features desk-diagnose-core/stack-budget assistant_data_store::tests::retrieval_native::memory_mutation:: -- --test-threads=1
LRDM_TEST_REDIS_URL=redis://127.0.0.1:6379/15 cargo test -p lcxl-remote-desk-manager --lib --features desk-diagnose-core/stack-budget automatic_ui_review_with_command_history_fits_small_stacks -- --ignored --nocapture
```

Redis 必须为隔离测试实例。Manager 的并发事务证明测试另使用 `LRDM_TEST_PG_URL` 指向隔离 PostgreSQL；不要指向产品运行库。测试网关和设备回执均为合成数据。

交互记忆的真实两阶段小栈组，以及 Signal 桌面审批/对象许可组分别串行运行，避免多个合成网关、小栈子进程和数据库夹具争用调度时间。其余用例继续并发；两宿主交互维护测试的阶段保护等待为 30 秒，覆盖 debug 数据库及模拟模型往返；该值不是产品超时。串行 CI 分组、生产任务边界及 1 MiB/2 MiB 限制保持。

## 报告与硬限制

`check.py` 先编译一个已知超过 128 KiB 的局部数组，确认当前 rustc 支持栈报告。它只给指定 crate 注入 LLVM remark；未使用的唯一 cfg 强制该目标重新 codegen，不清空依赖缓存。解析流只保留紧凑报告。构建未成功、产物仍为 fresh、零记录、符号格式不支持、关键族缺失都会失败。编译备注不包含 prompt、会话或凭据。保护范围还包含共享 Facade 的会话分页、定位及引用装配，必须采到三个入口的实际函数帧，不能以 DTO 转换或析构帧替代；两个依赖上下文分别采集 debug/release。保护范围包含共享 `ai_assistant` 纯逻辑、`ContextProtectionSet::validate_references`、两个宿主的全部 `assistant_data_store` 与原输入台账 `agent_run_event_store`。报告必须实际包含保护引用校验、记忆提炼、worker、Goal 原收尾与当前会话快照接纳。新增记忆路径同样受单帧上限约束；旧范围的报告不能证明新路径通过。 后台记忆 worker 必须包含 `advance_one_inner` 的实际 poll；元数据 GC 必须包含原事务、收录清理及原生授权/发布查询的实际 poll 帧。只有 future 构造帧或析构帧不算覆盖，解析器包含两者的失败反证。GC 以调用方持有的执行阶段返回，保留原事务、Send 和取消所有权，不新增运行时任务；阶段擦除前仍检查 16 KiB 原始 future。

`run.py` 对当前构建只施加 `report.HARD_FRAME_LIMIT` 定义的 128 KiB 单帧硬上限及必检执行覆盖。阶段原始 future 上限为 16 KiB；默认线程栈保持 2 MiB，显式 1 MiB/2 MiB 组合回归仍须通过。不要求新代码比旧代码使用更少的栈，不执行单函数、调用族、提取谱系、历史基线或 HEAD 的相对预算比较。

报告继续记录工具链、目标、Cargo profile、特性、Rust flags、构建配置与 lockfile 指纹。debug/release 和 Web/Manager 依赖上下文分别采集，不能以另一份报告代替当前构建的验证。`stack-budgets/` 中既有记录仅作历史分析，不参与门禁，也无需为函数增长重新分配历史额度。

`--report-only` 只采集证据，不表示门禁通过。常规入口默认执行固定硬上限和必检覆盖；构建失败、缓存产物、空报告或缺少实际执行帧继续失败。
优化构建必须保留必检函数的真实执行证据。纯校验可保留独立的编译边界；异步入口沿原调用点返回有 16 KiB 检查的类型擦除 future，继续由原调用者驱动。仅给 `async fn` 构造器标注 `inline(never)` 不能证明其 poll 未被内联；不能删除必检模式或只登记构造/drop 帧来通过。

历史写入终态收尾还必须包含实际原生 producer 加载、重验、job 终态核验及已发布回执读取的执行帧。元数据路径的必检正则从对应宿主命名空间起匹配，构造函数和 `core::ptr::drop_in_place` 均不能代替执行帧；解析器保留这两类反例。

发布任务输入须覆盖 `TaskPromptRetention::validate`、完整标签识别和两宿主原需求证明准备/冻结/原实例读取及 `native::inputs::published_on` 的实际 poll。原模型完整来源在原创建事务内核验，首次接纳时确认实际来源与保存策略，最后配额后不再查询。本人需求不自动成为长期事实；私有、未知、删除或已过硬期限的 AI 需求不能借发布确认获得新副本。原实例快照与后来编辑分别验证，并执行显式 1 MiB/2 MiB 组合及擦除前 16 KiB 检查。

运行态控制来源注册须覆盖共享 `source_registry::snapshot::registered_inputs` 和 `native_control` 的实际执行帧。控制仅在原始快照包含全部真实父输入、保留期限及披露限制未被削弱时取得派生文本策略；每份父来源仍单独接受原生检查。显式 1 MiB/2 MiB 组合覆盖实际生产标签、倒序多层依赖、模型父档案、私有/Secret、缺失父来源及循环，不能用来源名白名单或省略父输入来代替闭合。

主会话的委派保存入口还须覆盖两宿主 `observation::save_main_delegation_session` 和 `save_main_history_on` 的实际 poll。历史准备、原子任务结果确认、原会话 CAS 与最后的历史写入在同一原事务中完成；保留原查询、完整解码和原始 JSON/version/lease 围栏，最后历史配额之后不再查询或写入原生来源。显式 1 MiB/2 MiB 组合覆盖真实保存、同线程重放、晚期历史/覆盖失败回滚及原生写入先于配额的顺序；Schema 和模型档案夹具放在限定线程之外，不能让测试运行时替产品另起任务。

定时定义的检索范围冻结必须覆盖 `ScheduleStore::create_draft_on` 与 `capture_retrieval_consent_on` 的实际 poll。范围来自原主会话的完整解码与原始行围栏，不由当前设置补发；实际创建、回滚、重试和修改继承在显式 1 MiB/2 MiB 线程上组合验证。该检查不证明真实 occurrence 已完成范围绑定、恢复或模型发送。

定时检索期限沿用原生运行边界：会话续跑没有固定运行截止时，保留已有范围期限；独立定时任务必须提供已发布合同的运行时限。范围中的可选期限不替代实际任务、当前租约及来源校验；Goal 的原生截止仍须单独生效，不把 `start_deadline` 当作运行截止。

会话续跑的首次领取和读取必须覆盖 `claim_continuation_on`、`assistant_retrieval_deadline_on` 及原任务/会话锁的实际 poll。首次真实领取只创建一份实例检索预算，审批续跑不重置账本。实际领取、写入失败回滚、重复检查及取消在显式 1 MiB/2 MiB 线程上组合验证，阶段原始 future 仍检查 16 KiB；模型编排回归另外验证审批消费后保留原预算。计算中的 `context_usage_basis` 不替代原生授权，其余原始会话字段、快照和配对租约继续校验。这不表示定时空闲恢复或完整三端流程已完成。

原任务/会话锁按现有锁序在同模块分为独立任务锁定、会话续跑校验两个执行阶段，`lock_fresh_action_session_on` 和 `validate_continuation_action_on` 同样必须出现实际 poll；保留原 SELECT、完整解码及双次 authority 检查。领取入口以立即构造并拥有执行 future 的工厂暴露真实 poll，不能只检查未被实际 codegen 的 async 构造函数。阶段不增加线程或任务，取消仍由原调用方持有。

领取事务还必须包含 `prepare_continuation_task_on`、`bind_continuation_session_on` 及最终领取的实际 poll。原任务和原始来源行由同模块私有状态拥有，原事务一直持有所有锁；阶段间既不重复拨付，也不把事务当作可省略原查询的快照。当前必检覆盖独立任务的定义个性化冻结、原事务创建、数据初始化、原行加载与绑定核验，以及原生动作与子任务等待的收尾。来源级联恢复已移除，快照接纳及当前调用核验替代对应执行覆盖，历史测量仅供分析。

上下文重新领取后的进程中断还须覆盖两宿主 `recover_action_free_fresh_task`、原生复核和结算阶段及审批/子任务恢复交接的实际 poll，以及共享 `validate_recovery_binding`。恢复核验原引用、输入、控制序号和原租约，不凭引用前缀重新规划。显式 1 MiB/2 MiB 组合保留执行中断、已保存答案、两份租约到期、篡改拒绝、配对事务回滚和幂等收尾的真实存储验证；原任务和检索预算不得重置。各阶段复用同一事务与原查询，通过调用方持有的 Send future 顺序驱动；交接前检查 16 KiB 原始 future，不新增生产任务。 原生主停止推进控制序号和租约后，旧上下文定位不能授予规划权限；原来源组已停止、输入一致且控制序号早于停止状态时，沿原生流程收尾，无子任务也不得挂起。原停止入口和子任务等待扫描必须出现实际 poll。1 MiB/2 MiB 组合覆盖等待及重新领取后的停止、重复控制、来源组不匹配拒绝、结算回滚和幂等；原生在途和未知结果另按原回执核对。

独立任务的共享执行入口还须在两个宿主报告中出现实际 poll；共享库中的构造帧不能代替宿主实例化的执行帧。初始上下文继续完整比较，个性化及检索元数据与原生领取时的独立快照比较，不从待检查对象补造期望值。没有原聊天的任务保留没有本人输入的状态，不新增聊天事件或补发检索许可；原生结果与派生元数据不因输入序号为零而伪造输入。新会话的真实领取、初始化、提交和完整解码在显式 1 MiB/2 MiB 线程内组合验证，原合同、租约及重复权限复核继续生效。

## 小栈与原始 future

两宿主的资料请求清单还须采到 `SessionSeam::assistant_data_request_manifest` 的实际 future poll。Release 可能将私有清单准备代码内联到这个入口；只有构造或 drop 帧不能代替执行覆盖。

压力测试使用独立测试子进程，在显式 1 MiB 和 2 MiB 线程上运行真实编排及审批链，启动时移除继承的 `RUST_MIN_STACK`。交互记忆用例另在同样的两档栈上执行原任务拨付、两次原文提炼调用、发布和结算，回环网关与原主会话夹具在限定线程之外准备，作业拨付、worker 和结果断言在限定线程顺序驱动，测试自身的大 poll 不叠加在 worker 调用之上。检查点核实实际执行线程；沿用生产已有任务边界，避免大型 fixture 叠加，也不能把产品 poll 转移到未限定线程来通过。原代码在 macOS ARM64 的 2 MiB 组合上有失败反证；其他平台不要求原代码稳定溢出。

`stack-budget` 是开发检查特性，启用后在类型擦除前断言阶段原始 future 不超过 16 KiB。类型布局、future 状态、单帧和完整组合是四项不同指标。跨线程后台工作继续使用保持 `Send` 的阶段，不以局部执行 future 破坏现有 `tokio::spawn` 契约。取消/drop 必须仍释放原 guard、租约和转发器。

完整会话 decode 保持事务内的完整验证。同事务复用证明只省 serde：scope、根行和 budget 仍强制 SELECT，核对原主体、version/lease、行身份与原始 JSON。PostgreSQL READ COMMITTED 下连续查询可能读取不同已提交版本；不能把同一事务等同于同一快照。证明仅持有实际事务引用，不跨会话写入、事务或请求缓存。

Signal 原始回执收尾还须包含原生会话完整读取、原命令核验、原能力动作核验、命令来源和电脑结果恢复的实际 poll。各阶段沿用调用方的原事务，保留全部查询与原始身份验证；读取仍逐次完整解码，失败时不把部分恢复的 after-image 覆盖原会话。

输入接收还须覆盖两宿主的 `admit_user_input`、`prepare_input_goals_on` 和 `write_input_goal_events_on` 的实际 poll。原入口持有事务与会话，Goal 准备阶段拥有待提交状态，Manager 的 `persist_input_on` 写入原生会话和输入事件并标记旧恢复，原入口处理 CAS 回滚或继续提交；原查询、锁、审批围栏和输入幂等保持。显式 1 MiB/2 MiB 组合覆盖普通/Goal 输入的现行状态、实际失败回滚、重放与原检索预算保留。Schema 夹具在限定线程外建立，夹具运行时继续处理连接池；产品接收、完整 decode 和事务断言都在限定线程中运行。各执行阶段在类型擦除前检查 16 KiB 原始 future。

历史最终落库须覆盖两宿主 `history::staged::write::write_on`、`write_conversation_on` 和 Signal 的 `write_binding_on` 实际 poll，阶段原始 future 在类型擦除前检查 16 KiB。写入只使用原准备阶段核验的来源、同事务自己的已提交单元及原 CAS，最终配额更新后不补发原生来源 SELECT。显式 1 MiB/2 MiB 组合覆盖多单元成功、首/中单元失败、重放和外层回滚；失败单元不得推进缓存、来源代次或留下绑定，成功前缀与未失败单元继续按原规则入库。


Manager 目录授权更新还须覆盖 `file_scope::update_file_scope_guarded`、`load_session_on` 和 `persist_update_on` 的实际 poll，入口、完整加载和持久化阶段均在类型擦除前检查 16 KiB 原始 future。原会话与准备后会话保持完整内容并有明确的堆所有权；owner/root/conversation 锁、原版本/租约、子任务准入、任务合同、回执指纹和时间校验保留。显式 1 MiB/2 MiB 组合覆盖选择/撤销/重放、批准与拒绝、旧模型版本和伪造任务来源拒绝，以及新/已有会话的事件失败回滚；只有原事务提交后才通知，冲突仍按原入口回滚重试。

共享 `desk-assistant-data-store` 单独采集 debug/release 报告，分别使用 Manager 和 OSS 的 lockfile、特性及构建上下文；宿主报告继续覆盖在宿主中实例化的共享泛型存储函数。共享包的非泛型快照解码、查询解码和控制回执投影必须实际出现在报告中。移动模块后仍检查实际执行帧和固定硬上限。共享包单测由两个宿主 CI 执行。

同一实体通过宿主重新导出后，宿主帧报告继续使用原实体路径作为稳定报告身份，并在 `entity_symbol_relocations` 记录实际新符号。测量字节数保持；两份不同实体映射到同一预算身份会失败。Signal 的本机配额游标扩展与共享计数视图是不同实体，不合并身份。

显式迁移到应用层的维护 worker，以及移到共享工具结果/事实存储的原函数，继续使用原逻辑报告身份；`operation_symbol_relocations` 保留物理符号，身份冲突会失败。此映射只覆盖已列明的迁移函数，新确定性发布路径保留独立执行覆盖。新增 `memory_commit::registered::try_publish` 必须采到实际执行 poll，构造器、析构帧和内层闭包不能代替；新包自身报告仍保留实际共享函数路径。




独立快照须覆盖共享 `ReadProjectionProof::snapshot_envelope`、`AssistantModelDataManifest::validate_for` 和两宿主 `visible_history::snapshot::prepare_on`、`native::resolve_policy_on`、`native::inputs::owner_text_on`、`prior_outputs::validate_on`、`model_gate::authorize_on`、`model_gate::output::validate_on` 的实际执行帧。异步入口必须采到 poll，构造或 drop 不算覆盖。快照在原会话 CAS 和事务中首次接纳，保存当前会话自己的内容与静态保存策略；后续只核验当前会话、原生调用和内容硬期限。原会话删除、普通留存到期或内容更新不触发上下文剔除、摘要撤回或专用恢复。显式 1 MiB/2 MiB 组合覆盖接纳前后删除、同调用重放、在途回答、后续读取和篡改拒绝，当前会话及原操作围栏继续生效。

共享维护单轮 `MaintenanceSweep::run_once_on` 以及 GC 的 `due_actors_on`、`claim_on`、`finish_quota_on` 须在两个宿主采到真实 poll。宿主保留定时器、原事务和取消所有者；共享单轮只合并分页、游标及回调顺序。配置的批次及领取上限不改变原生调用身份和结算；计数准入与已保存回执验证分开。共享单轮及 GC 游标提交的原始 future 检查 16 KiB，并在显式 1 MiB/2 MiB 线程上组合执行。

遗忘提交按普通工具协议返回模型：共享 `commit_memory_forget_group`、`read_memory_forget_group`、`dispatch_forget` 的真实执行帧，以及两宿主原生工具事务 `memory_control::native::forget_on` 的实际 poll（release 会将提交子 future 内联到此帧） 必须纳入固定硬限制检查。提交保留原活动 turn、租约、输入水位与预算，只刷新下一请求的当前记忆准备状态；真实 1 MiB/2 MiB 组合覆盖提交、回执重放、记忆刷新和后续模型回复。没有遗忘专用空 answer 或会话终态。

不可恢复会话的管理投影不进入执行读取。两宿主的 `retention::close_root_for_subject_on`、`retention::close_child_on` 和 `management::retire_unsupported_for_expiry` 须采到实际执行 poll；由原调用方持有的移除与回收阶段检查 16 KiB 原始 future，不创建新的生产任务。显式 1 MiB/2 MiB 组合覆盖旧父子会话关闭、原始 JSON 保留、版本与租约围栏、事务回滚和有效租约保护。过期分页、原生未知结果及待结算用量另用真实存储回归验证。

自动审批授权提交须覆盖共享 `permission_grant::apply_automatic_grant_failure`、Signal 的 `decide_permission_request_with_authority` 和 Manager 的 `decide_with_authority` 的实际执行帧，异步事务不能仅采到构造或析构。显式 1 MiB/2 MiB 组合执行命令绑定条件改变后的系统拒绝，保留原模型判断、用量和事务续跑；人工决定与可重试故障不转为系统拒绝。子调用的 future 仍由原事务调用方持有，原 SELECT、授权核验、版本、租约、锁顺序和取消所有权保持。

根会话等待扫描须覆盖两宿主 `candidates::root_wait_candidates` 的实际 poll，候选阶段在类型擦除前检查 16 KiB 原始 future，并执行显式 1 MiB/2 MiB 组合。格式版本的 SQL 筛选、无子任务的组排除、每页根会话去重及原始游标均属只读提示；实际恢复继续保留原写事务、全部查询和授权围栏。文件型 WAL 回归分别验证写入者存在时只读扫描仍可完成，以及释放写入者后快照和旧格式删除可正常提交。

会话的 `conversation_schema_version` 与并发 `version` 分开维护。正常创建写入共享核心的 `CONVERSATION_SCHEMA_VERSION`，不兼容的持久化格式改动必须递增该常量。格式分类只比较 `conversation_schema_version` 与当前格式常量，不以解码失败、缺字段或旧字段推断旧格式。执行候选用可索引的格式列筛选，完整读取仍验证原 JSON；当前格式的数据异常独立处理，不降级为旧格式。管理、删除、回收及原生结算保留旧格式记录的处理入口。

会话标题须覆盖共享标题请求、解析、回退与 `agent_loop::title::generate`，以及两宿主 `agent_session_store::title` 读取预算和条件写入的真实执行 poll。命名沿用原调用方、租约、预算及模型计量，原回答先保存，标题失败保留回退。标题阶段和存储阶段在类型擦除前检查 16 KiB 原始 future；显式 1 MiB/2 MiB 组合通过两宿主真实模型出口与合成网关执行完整命名及原生台账写入，不新增生产任务边界。

工具模型视图覆盖共享请求投影、工具正文投影和会话短 ID 恢复的实际执行帧，资料、文档、结果引用及附件分页模块均纳入固定单帧上限。release 中内联的领域投影由其实际工具正文调用入口计量；不以构造或析构帧代替执行帧，也不为采样另起生产任务。

记忆历史读取还须覆盖两宿主 `memory_history::read_on`、`memory_view::read_history_on`、`read_proof_on` 和宿主实例化的共享 `memory_history::head_on`、`page_on`、`scan_on` 的实际 poll，以及共享 `fit_memory_history`。新读取阶段在类型擦除前检查 16 KiB 原始 future；显式 1 MiB/2 MiB 组合覆盖精确旧版读取与重放，默认线程栈保持 2 MiB。查询沿原事务和模型用途读取围栏提交，不创建新生产任务；游标、原头版本界限和来源证明仍保存在共享存储中。

记忆写入来源档案重验、回答引用读取及实际消费来源的定位恢复也须采到两宿主的实际 poll。重验保留原始档案、父链接和原生回执查询；只有原始行完整相等时才复用已解码证明。引用元数据读取使用原有线程和调用方 poll，阶段擦除前检查 16 KiB future；显式 1 MiB/2 MiB 组合须覆盖缺少正文留存策略及可选引用存储失败的实际恢复。

同步记忆写入覆盖两宿主 `assistant_data_store::memory_write::write`、`write_on` 及共享 `memory_write::apply_on`、`sources::load_on`、`receipts::collect_on` 的实际 poll；显式 1 MiB/2 MiB 组合执行真实更新、提交与幂等重放。原始 future 在类型擦除前检查 16 KiB。模型提炼与关系维护 worker 已移除，相关旧符号不作为现行门禁目标；正常对话、审批、来源围栏及输出清理继续覆盖。

同步写入的 `sources::load_on` 与提交回执 GC 的 `receipts::collect_on` 在 debug 中要求独立的实际 poll；release 将其内联时，分别要求原宿主 `memory_write::write_on` 和 `output_archive::gc::collect_actor_transaction` 的实际 poll，并将内联后的完整调用者帧纳入 128 KiB 上限。构造和析构帧均不能代替执行证据；独立原始 future 与真实小栈行为检查继续适用，不改变优化级别或生产任务边界。


模型参数错误反馈还须覆盖共享协议的 MemoryDraft、MemoryWriteRequest、ConversationSearchRequest 原校验函数，以及核心 `model_input::check_at` 与资料工具解码错误的实际执行帧。两宿主依赖图分别采集 `desk-agent-protocol` 和 `desk-diagnose-core` 的 Debug/Release 报告；协议 Debug 还要求独立合计容量 helper，Release 将其内联时由实际公开校验入口覆盖。原始 future 16 KiB、单帧 128 KiB 与默认 2 MiB 保持。无效遗忘参数在实际 1 MiB/2 MiB 模型循环中核验字段错误会交回模型，且无操作派发；模型 HTTP 出口另外核验记忆合计容量和解码失败均没有写入。

记忆工具的参数拒绝、批次拒绝和提交回执继续保留实际父来源及其留存约束。两宿主 `visible_history::native::resolve_policy_on` 必须采到真实执行 poll；每个父输出保留原 SELECT，只读归档头部，写入时确定的留存结果不再递归重算。显式 1 MiB/2 MiB 组合覆盖错误反馈后的新增和更新。

模型调用校验父输出只读归档头部和收据小列，每次 SELECT 保留，不加载证明正文或条目。历史读取、引用等读取完整证明的路径还原条目并校验指纹。

定时工具的原始参数保留须覆盖共享 `ui_model_ids::resolve_session_call_with_context`、定时提议 Schema/解析及两宿主 `manage_schedule_tool` 的真实执行。创建、查询与取消仍按原始调用、会话版本和租约核验，不用 JSON 重序列化替代原参数；输入错误与会话不可用分别处理。显式 1 MiB/2 MiB 组合通过真实模型出口覆盖非法周期规则的反馈、原始格式查询及待演练草稿创建，保持 16 KiB 原始 future 和 128 KiB 单帧上限。

定时任务的参数表示转换还需覆盖共享读取摘要与试运行来源核验的实际函数、Debug 审批暂停 poll，以及 Manager 读取完成事务的实际 poll。审批暂停在原始调用上唯一定位、复用既有引用/内容比较，读取摘要复用同一规范化规则；显式 1 MiB/2 MiB 组合验证格式差异、真实参数变化、重复调用和保存失败回滚，原模型参数保持原样。Release 内联的审批逻辑沿原调用方阶段验收，不增加生产边界或改变优化等级。

租约续租还须覆盖两宿主实例化的共享 `desk_signal_facade::service::lease_heartbeat::renew_until_lost` 实际 poll。复用原心跳任务与 guard，擦除前检查 16 KiB 原始 future；确定性时钟测试覆盖暂时数据库故障、恢复、真实失去租约和卡住的存储，失败重试不得越过最后确认的租约。普通会话的子任务通知更新投影，显式等待恢复继续原任务并核验原授权。工作台报告可在原工作及原预算内驱动有界协调，审批恢复优先，重复或迟到通知仍按持久报告状态和原来源判断。保留已发布定时合同的报告收尾边界。
