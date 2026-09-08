# AI Security Model

Remote desktop with AI that reads system state is powerful — and demands a strong trust boundary. LCXL Remote Desk treats AI as a first-class control plane, governed by invariants that are **security-relevant: breaking them is a regression**.

## The Server Is the Sole Source of Truth

All authorization logic is verified **server-side** by the central signaling brain. Fields like `request_id`, `target`, `actor`, `scope`, `caller`, the final `risk`, and `approval_id` are injected and validated by the server — a control plane (browser, mobile, or MCP) can **never self-report** them. The browser-side request body does not even contain these fields structurally. The trust anchor is connection authentication: a bare relayed connection is never promoted to an authorized one.

## Capability Protocol Is Device-Facing

The capability protocol describes **what can be done to a device**, independent of who is calling. Read-permission points are **derived from the input** (`OperationInput::capability()`), which prevents drift between capabilities, evidence collection, and audit.

## Central Grant and Local Ceiling

The open-source Signal's central grant defaults to **confirm each action**, while each device keeps an independent **local execution ceiling** whose default remains **suggest-only**. The effective mode is the more restrictive of the two, so a device can narrow the central grant but never widen it. Raising the local ceiling never enables unattended execution: every command still requires explicit, **server-mediated confirmation**.

Both the **AI diagnosis panel** and the **terminal AI copilot** share one sealed confirmation chain. The copilot itself stays suggest-only — it never executes anything on its own. For a `confirm_required` suggestion, the operator may explicitly **promote it to execution**: that click relays the exact command to the host, which **re-classifies it server-side** (a control plane's self-reported decision is never trusted), mints the `exec_request_id`, and returns a preview the operator must **approve** before anything runs. Execution is gated by the same **local execution ceiling** — left at suggest-only it is off, so the Run action returns a non-executable preview that guides the owner to raise the ceiling first. Raising it opens confirmed execution for every AI surface on that device (diagnosis and copilot alike), not the copilot alone.

## Scoped AI Assistant Grants

When AI Assistant presents a scoped permission request, the owner must approve or deny each item. Approval may only remove resources, operations or export destinations, shorten the lifetime, or reduce the number of uses. The server rechecks the request revision, capability contract and current readiness; it never restores a scope the owner removed. Exact actions remain bound to server-frozen input, and approval records authority rather than dispatch or successful execution. Servers without the corresponding control path do not expose the operation.

For open-source Signal reads, the SQLite Prepare/DispatchIntent/outbox record is also the single-send fence. Signal checks the claimed dispatch, current input, grant revocation/expiry and readiness before sending and again before releasing a result. Typed limits narrow the host request and bound complete success or error payloads. Result labels bind the exact validated bytes and cannot outlive the grant or selected data. A Provider response that arrives after authority changes remains a known durable outcome, but its content is not passed to the model and it is never treated as permission to retry automatically.

## Owner-Interactive Free-Form Commands

Template matching remains the default admission policy. A trusted central brain may explicitly grant `OwnerInteractive` only to the authenticated owner acting on that owner's own device. The open-source signal's single authenticated account is its owner subject. Non-owners, shared/access-code sessions, organization members acting on shared devices, fleet execution, automation, MCP, and raw agent requests remain template-only or disabled.

An off-template owner command is not declared safe. The blocklist is a broad, best-effort prefilter rather than a complete semantic sandbox. Every such command is therefore classified **Critical**, shown in full with its shell/cwd/timeout and a “blocklist only” warning, and requires a one-shot explicit approval. The approval action has no default focus or Enter default. The model may propose and wait; it cannot approve.

The approved draft is reclassified before dispatch and must match field-for-field. The edge then independently checks the authorization binding, current local execution ceiling, blocklist, admission basis, limits, and sealed plan before the worker receives only frozen `program + argv`. `cmd.exe` and zsh free-form commands are not admitted in the first release.

For agentic execution, the edge cannot observe the browser click itself. It trusts the authenticated central stamp to mean that the central consumed a valid one-shot approval; compromising manager or the owner's OSS signal therefore compromises that approval boundary. The edge still prevents untrusted-source forgery and transport/plan drift, but resisting a compromised central would require a separate host-local approval proof.

## The device's own concurrency ceiling

A host also caps how many commands may run **at the same time**
(`ai_policy.max_concurrent_executions`, default 4). The device enforces this
itself rather than trusting the caller to respect it.

A central manager schedules against its own quota too, but that only binds work
the manager dispatched — a control end reaching the device through an
open-source signal server never goes through the manager at all. The device
therefore keeps its own ceiling: whether a command is admitted does not depend
on who scheduled it.

A command over the ceiling is **refused without being accepted** — it is not
recorded in the device's execution ledger, so a later retry is admitted normally
rather than mistaken for a redelivery.

## A Running Command Reports on Itself

Once a command starts, the host **says so**. It reports that the command was
accepted, reports periodically that it is still running, and answers a direct
question about any dispatch it was ever told about.

This replaces inference from a clock. Previously a control plane that heard
nothing had to guess whether a long command was still working or had been lost,
and a wrong guess about a command that changes the system is not a cosmetic
error. Now silence is never interpreted: the authoritative answer is always the
host's own durable record of that dispatch, which survives the process that
wrote it.

A host that lost track of an execution across a crash says exactly that —
**indeterminate** — rather than claiming it failed. A command that may have
changed the system is held for a human to look at, never quietly retried.

## Stopping a Command

A running command can be **stopped**, and stopping it reclaims the whole process
tree — not only the process that was launched. A command that starts a helper,
forks, or backgrounds work cannot leave that work running behind it. Because the
host reclaims a container rather than signalling a process, a command that
ignores signals is stopped just the same.

Every stop is **recorded**, whether or not it landed, and the record attributes
it to the authenticated operator — never to a name supplied in the request.

A stop is a request, not an outcome. The command is not treated as over until
the host reports its own ending, so a stopped command that had already made a
change is not misreported as one that never ran.

## Computer Action executor acceptance

OSS SQLite stores `agent_permission_resume`, a continuation fence referencing the immutable original decision event. It contains identity and claim metadata, not another copy of conversation content or an executable device task. New decisions, grants, session changes and the pending fence commit atomically. Claim checks the original decision, input revision, prepared session version and current grant records in one transaction before recording `started`. Readiness or source rejection does not renew authority. Started continuations are never reset to pending; expired original leases use the existing action ledger, and an old candidate cannot recover a newer input's turn. Session retention deletes its continuation records in the same transaction as related events and grants. This is logical cleanup, not forensic erasure.

Confirmed scheduled conversation continuations are scanned and dispatched by the single-instance Signal central process without a connected browser. Dispatch requires a unique online device, an enabled Assistant switch, fresh capability readiness, and an atomic claim of the original session and occurrence. Historical scope is only a ceiling; model and tool calls still check current authorization. Scheduled permission waits use a separate claim path and continue the original input after a decision. Repeated scans do not reclaim started work. A persisted failure can be settled, but transport errors and unresolved action outcomes do not authorize a failed-result retry. When the original turn has persisted a success or failure and every action has a known terminal outcome, the central process can finalize its unsettled occurrence after the original lease expires. Recovery neither calls the model nor dispatches actions or increments attempts. Older-turn answers, running sessions and unresolved execution states are not terminal evidence. Ordinary session retention does not independently terminate scheduled work; the conversation and its occurrence follow the same recovery process. A persisted exact permission pause is restored to its original request after restart without another model call or approval request. An already-recorded user decision still passes current authorization checks before continuation. After the original execution leases expire, an interrupted turn with no tool calls, action records or pending approvals is settled as failed. If cancellation was requested, it is settled as cancelled without increasing the failure count. Neither case reruns automatically; turns with tools or actions still require original-result reconciliation.

Scheduled task requirements are limited to 16 KiB of UTF-8 text, matching conversation input admission. The limit is checked when creating or editing a task.

On the Automations tab of Scheduled tasks, select **Guided run and permissions** to retrieve the latest guided run, including pending, running and cancelled attempts. Completed runs show the scope successfully used and counts of unconfirmed or unclassified calls. Times use the selected display time zone. Viewing or refreshing this history does not start a task or grant permission for future automatic runs.

When the device is online, select **Prepare guided run**, then **Open guided run**, and finally **Start guided run** in the Assistant. Preparing or opening the conversation does not execute the task. The run uses the saved task text and may send messages or modify files; actions still follow current permission checks. Return here afterward to refresh the permission history. The current guided-run entry fixes the initial task input and does not yet support additional clarification messages. Completing a guided run does not automatically publish a recurring task.

Select **Run history** on a task to browse scheduled and manual occurrences, their status, request/scheduled/start/end times, cancellation requests, and missed counts. Times use the selected display time zone; refresh reloads the newest records. Reading history does not execute a task. Known failure or interruption reasons are shown. Missing or unrecognized reasons display as unavailable; raw internal errors are not exposed. For a started occurrence, select **View conversation** to read its original conversation, mark messages from this run's initial turn, and load earlier messages. The server rechecks access; in Manager, a device transfer or deletion prevents its former owner from reading the conversation. Opening history or an occurrence keeps its public task/run identifiers in the address bar, so the page can be bookmarked or linked. Reopening still checks current access. Viewing alone does not execute or resume a task. If this is the current run and its device has one online connection, you can review pending requests, narrow their permissions, and approve or deny them here. The server rechecks the current run and original request; recording a decision may allow execution to continue. The page reloads the conversation, and a recorded decision is not shown as successful task execution. If the response is uncertain, review the latest state before resubmitting; the page does not retry automatically. Reconciliation actions are not provided here yet.

Select **Cancel unstarted guided run** to withdraw a reservation before it starts, even if the device is offline. This button is unavailable once the run has started; withdrawing a reservation cannot stop actions already in progress.

If a started guided run is cancelled before any tool call, the server can return the task to draft after checking the persisted session and action records, while keeping the cancelled attempt. Runs with tool history or unresolved actions require reconciliation and are not automatically rerun because a cancellation result arrived.

When the initial guided turn fails without tool calls or action records, the server can mark the attempt as failed and return the task to draft. You can prepare another guided run; the old conversation is not restarted and failure does not automatically trigger another execution. Errors before a session is persisted, runs with tool history, and unknown outcomes still require reconciliation.

If a session has ended but a service restart left its guided-run record unfinished, background recovery checks the saved session to complete the success, failure or cancellation record. Recovery does not call the model or execute actions again. Active sessions, unresolved actions and incomplete evidence remain pending reconciliation.

Scheduled recovery for native Computer Actions validates the original dispatch binding and device receipt within the same database transaction. Completed actions contribute only their original result; actions with a valid accepted background execution continue waiting for their original receipt. An unresolved outcome retains the original action identity and pauses future scheduling, even when cancellation was requested. Recovery neither redispatches the action nor refunds consumed permission uses. Provider proposals without a dispatch intent use separate reservation checks. Only matching original calls, input digests, grants and unissued states with no dispatch outbox record allow the proposal to close and its unused reservation to be released in the same schedule-settlement transaction. A committed use cannot be refunded this way. Failed schedule settlement rolls back both changes. Exec commands with a frozen origin use their separate command identity to verify the original turn, input, dispatch generation and result. The receipt and related action completion join the schedule-settlement transaction. A command awaiting its original result before its deadline keeps the occurrence running; uncertain dispatch or an expired deadline becomes unknown and pauses scheduling. The ordinary result publisher cannot independently change an active scheduled conversation, even after its lease expires. Recovery neither resends commands nor adds an automatic follow-up. Original exec delivery consumption commits with schedule settlement. A publisher holding an older snapshot rechecks consumption, so a foreground copy of the same receipt is not delivered again under another message ID. For registered read or capture tools without a durable action or command record, recovery closes an unresolved call as result unavailable, without claiming non-execution or success. Already-persisted read results stay unchanged. Mutations, export operations and unknown tools do not use this path. The notice commits or rolls back with schedule settlement, and recovery does not repeat the read. A committed dispatch intent without a command origin or Computer Action binding requires matching original call arguments, grant subject and committed reservation. Its pending or claimed original outbox becomes outcome unknown within schedule settlement, retaining its Provider identity and pausing future scheduling without refund or redispatch. Unverifiable ledgers and existing results that cannot be validated still require separate reconciliation.

Before the first release, single-instance Signal does not automatically migrate SQLite databases from earlier development versions. An empty database receives the current complete schema. An existing database must have the current schema version and all required tables and columns; otherwise startup fails without deleting, rebuilding or repairing application data. Do not change the database version number to bypass validation. Capability grants use one current format; obsolete formats are neither converted nor reauthorized.

A worker sets `executor_accepted=true` in `ComputerActionStarted` only after the original action passes preflight and acquires the writer lease. Missing fields mean false. Legacy `MayHaveStarted`, a successful send, and timeout are not acceptance receipts. The marker indicates executor ownership, not native effects, eventual success, or permission to retry after restart. Daemon and worker must use matching builds because their IPC is binary.

The shared loop labels background/unknown/wait statuses as central-control information inheriting the original message boundary, not as native results. Receipt-bearing late completion preserves the original call, digest, label, and stable completion ID. The wait response is separate from that result, whose delivery is acknowledged only after a successful save. Cancellation support is separate from successful acceptance or completion delivery.

OSS Signal freezes the original side-effecting Computer Action plan, connection and model-call provenance on its existing dispatch outbox before sending. Only an explicit acceptance from the token-authenticated host with matching audience, frame, action and generation is persisted. Duplicate acceptance does not renew its timestamp or authorize another send. Browser snapshots and element waits share the transport but remain inline reads: they do not require mutation-result provenance or gain background-mutation acceptance. The current SQLite structure includes nullable binding, acceptance and promotion metadata, plus independent owner-snapshot sequence/cache columns. Missing metadata does not establish acceptance or export authority, change execution leases, or prove that an action is running.

For those bound mutations, the completion observer validates the original authenticated transport identity and sealed plan, then atomically saves the bounded typed native result, tool projection and original receipt on the existing work record before notifying a foreground waiter. Result format 2 needs no additional database columns. The first unknown observation is retained; a real terminal result may refine it, but duplicate delivery does not refresh timestamps and conflicting terminal reports cannot overwrite it. Recovery, active waiting and the completion publisher retain the original model-call identity and labels, without another dispatch or implicit export permission. Outlook assistive handoffs remain unverified and manual-send-only; Gmail/Slack handoffs still require exact read-back.

Background promotion requires the original acceptance and frozen execution policy after the foreground budget but before the original hard deadline. It stores metadata on the same outbox, not a second executable work or renewed grant. The owner snapshot reads session, grants and old/new task projections in one SQLite transaction; its presentation cache is independent of execution CAS. Recovery correlates the original action instead of treating a missing legacy command row as proof of non-execution. A pending or corrupt page cannot starve all later receipts in the completion scan.

Original OSS task cancellation stores the request ID, reason digest and original-work stop markers atomically; duplicate requests do not renew timestamps and conflicting bytes fail. Reason text is neither persisted in the outbox nor forwarded. The single-instance sender validates the frozen host connection, audience, approver and action generation, rechecks after acquiring the socket, and retries only stop frames until an exact successful `ComputerActionStateReported` observation or original terminal result. It never follows a reconnect, refreshes execution authority, refunds a committed grant, or treats a socket write as an ACK. CancelRequested is not terminal; an original `PausedByUser` receipt can project Cancelled without claiming that previous OS effects were undone. SQLite reopen, concurrent cancellation/completion and loopback ACK-loss tests cover this path; full mobile recovery and other Providers retain separate acceptance gates.

Tool-free automatic interpretation reuses the foreground's strict model sink and audit checks. Before dispatch, the original binding freezes the selected sources, exact model destination and a digest of context/policy; the selection is bounded by five minutes and any earlier attachment/scope expiry. Completion does not mint that authority retrospectively. The current subject, original receipt, input/chain, context/policy, model revisions and expiry are rechecked before model I/O. Secret or invalid bytes remain non-exportable; original receipt labels are not overwritten by a model projection. If replay/retention filtering removes the result, no uninformed model response is requested. The result remains owner-visible even when no automatic explanation is authorized.

Model context filtering reserves 60 seconds of retention headroom for historical turns. Current-turn observations use their actual expiry: a short remaining lifetime alone does not remove them, but expired current input blocks the model call. Request and response checks keep the original expiry; context compression never renews it.

Idle-session retention removes the session and its action results, dispatch bindings, grant reservations, grants, run events and legacy command records in one transaction. Eligibility is rechecked inside that transaction, and a deletion failure rolls back the whole session batch. A late result cannot recreate a removed original work record. This is logical database cleanup, not a promise of forensic erasure from SQLite files, backups or storage media.

Snapshot reads and background-task stops can recover an offline target using the original opaque `sessionId` returned by a snapshot or history. This only locates the original actor/device session; the store still validates that subject and its persisted state. A known non-host connection is rejected, and recovery never authorizes a new turn, grants permission or redirects a stop to a new connection. Native foreground polling and duplicate-stop guards do not imply process-recreation or real-device lifecycle acceptance.

## Redaction Fails Closed

The pipeline runs **collect → redact → model → render**. Collection and redaction happen on the **edge device**; the model call happens **centrally**. If redaction fails, the edge returns an error and the central brain aborts **before** the model is called. Evidence (with raw screenshot bytes stripped) is always redacted before it leaves the host or reaches the model.

## API Keys Are Server-Side Secrets

AI model API keys live on the **central signaling server**, never on a thin edge device. They are **never** returned to the browser, **never** included in any public settings DTO, and **never** written to logs.

## Audit Records Metadata Only

Audit trails log content-free summaries — counts, sizes, token usage, provider, adapter. Raw prompts, outputs, and screenshots are **never stored in audit records**. This is distinct from access-controlled conversation and execution-result storage used for recovery and governed by retention.

## MCP Is Read-Only

The MCP tool set is a **static whitelist** with no execute / write / control tools by construction ("undefined means unreachable"). There is no diagnosis tool either: MCP exposes only read-only context, never model inference or screen capture. See the [MCP Server](/features/mcp-server) page.

## Model Agnostic

Wire protocols are isolated behind adapters on the central brain (OpenAI-compatible chat completions and Anthropic Messages). Adding a provider means adding an adapter — the orchestrator does not change.

Provider reasoning required for tool-call continuity is stored only as an opaque, source-bound replay envelope. It is never exposed in transcript DTOs, audit content, content-safety prompts, or logs. If replay is unavailable or belongs to a different endpoint/protocol/model revision, the complete affected tool group is omitted from the model-facing context rather than reconstructed or partially sent; the user-visible conversation remains intact.

## AI Interaction Disclosure

The AI diagnose and Terminal Copilot panels disclose, from the first interaction and for every session, that you are interacting with an AI assistant. The notice is a standing element at the top of each panel — never a one-time, dismissible banner — kept clearly distinguishable from the separate accuracy reminder ("AI can make mistakes"). This makes the AI's identity explicit rather than merely implied by naming.

## AI-Generated Content Marking

AI-generated output carries machine-readable provenance on its wire frame and a visible "AI-generated" marking in the UI. The marking is driven by the content being AI-generated, not by the provenance metadata being present, so a missing or stripped provenance never downgrades content to "not AI" (fail-closed). Provenance, when known, records which model produced the content and when.

This covers every surface that shows model text: the diagnosis answer, the terminal copilot answer, the inline command completion, and the provider connectivity-test reply snippet. A model-generated command suggestion is a novel output, not an assistive edit of what you typed, so it is marked; the completion's zero-latency local guess from your own recent history is not AI and is not marked.
