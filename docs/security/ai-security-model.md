# AI Security Model

Remote desktop with AI that reads system state is powerful — and demands a strong trust boundary. LCXL Remote Desk treats AI as a first-class control plane, governed by invariants that are **security-relevant: breaking them is a regression**.

## The Server Is the Sole Source of Truth

All authorization logic is verified **server-side** by the central signaling brain. Fields like `request_id`, `target`, `actor`, `scope`, `caller`, the final `risk`, and `approval_id` are injected and validated by the server — a control plane (browser, mobile, or MCP) can **never self-report** them. The browser-side request body does not even contain these fields structurally. The trust anchor is connection authentication: a bare relayed connection is never promoted to an authorized one.

## Capability Protocol Is Device-Facing

The capability protocol describes **what can be done to a device**, independent of who is calling. Read-permission points are **derived from the input** (`OperationInput::capability()`), which prevents drift between capabilities, evidence collection, and audit.

## Central Grant and Local Ceiling

The open-source Signal's central grant defaults to **confirm each action**, while each device keeps an independent **local execution ceiling** whose default remains **suggest-only**. The effective mode is the more restrictive of the two, so a device can narrow the central grant but never widen it. Raising the local ceiling never enables unattended execution: every command still requires explicit, **server-mediated confirmation**.

Both the **AI diagnosis panel** and the **terminal AI copilot** share one sealed confirmation chain. The copilot itself stays suggest-only — it never executes anything on its own. For a `confirm_required` suggestion, the operator may explicitly **promote it to execution**: that click relays the exact command to the host, which **re-classifies it server-side** (a control plane's self-reported decision is never trusted), mints the `exec_request_id`, and returns a preview the operator must **approve** before anything runs. Execution is gated by the same **local execution ceiling** — left at suggest-only it is off, so the Run action returns a non-executable preview that guides the owner to raise the ceiling first. Raising it opens confirmed execution for every AI surface on that device (diagnosis and copilot alike), not the copilot alone.

## Scoped Device Assistant Grants

When Device Assistant presents a scoped permission request, the owner must approve or deny each item. Approval may only remove resources, operations or export destinations, shorten the lifetime, or reduce the number of uses. The server rechecks the request revision, capability contract and current readiness; it never restores a scope the owner removed. Exact actions remain bound to server-frozen input, and approval records authority rather than dispatch or successful execution. Servers without the corresponding control path do not expose the operation.

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

A worker sets `executor_accepted=true` in `ComputerActionStarted` only after the original action passes preflight and acquires the writer lease. Missing fields mean false. Legacy `MayHaveStarted`, a successful send, and timeout are not acceptance receipts. The marker indicates executor ownership, not native effects, eventual success, or permission to retry after restart. Daemon and worker must use matching builds because their IPC is binary.

The shared loop labels background/unknown/wait statuses as central-control information inheriting the original message boundary, not as native results. Receipt-bearing late completion preserves the original call, digest, label, and stable completion ID. The wait response is separate from that result, whose delivery is acknowledged only after a successful save. This protocol support alone does not connect actual OSS generic background-task creation and cancellation.

OSS Signal freezes the original Computer Action plan, connection and model-call provenance on its existing dispatch outbox before sending. Only an explicit acceptance from the token-authenticated host with matching audience, frame, action and generation is persisted. Duplicate acceptance does not renew its timestamp or authorize another send. SQLite schema 8 adds nullable binding/acceptance metadata in place; existing rows are not backfilled, reauthorized or turned into running tasks. Acceptance storage alone does not yet provide the complete generic background completion/cancellation workflow.

## Redaction Fails Closed

The pipeline runs **collect → redact → model → render**. Collection and redaction happen on the **edge device**; the model call happens **centrally**. If redaction fails, the edge returns an error and the central brain aborts **before** the model is called. Evidence (with raw screenshot bytes stripped) is always redacted before it leaves the host or reaches the model.

## API Keys Are Server-Side Secrets

AI model API keys live on the **central signaling server**, never on a thin edge device. They are **never** returned to the browser, **never** included in any public settings DTO, and **never** written to logs.

## Audit Records Metadata Only

Audit trails log content-free summaries — counts, sizes, token usage, provider, adapter. Raw prompts, outputs, and screenshots are **never** persisted.

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
