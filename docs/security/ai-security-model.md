# AI Security Model

Remote desktop with AI that reads system state is powerful — and demands a strong trust boundary. LCXL Remote Desk treats AI as a first-class control plane, governed by invariants that are **security-relevant: breaking them is a regression**.

## The Server Is the Sole Source of Truth

All authorization logic is verified **server-side** by the central signaling brain. Fields like `request_id`, `target`, `actor`, `scope`, `caller`, the final `risk`, and `approval_id` are injected and validated by the server — a control plane (browser, mobile, or MCP) can **never self-report** them. The browser-side request body does not even contain these fields structurally. The trust anchor is connection authentication: a bare relayed connection is never promoted to an authorized one.

## Capability Protocol Is Device-Facing

The capability protocol describes **what can be done to a device**, independent of who is calling. Read-permission points are **derived from the input** (`OperationInput::capability()`), which prevents drift between capabilities, evidence collection, and audit.

## Suggest-Only by Default

The default execution mode is **suggest-only**: the model can propose commands but cannot execute them. Higher-risk actions require explicit, **server-mediated confirmation**. The granted mode is set centrally, and each device further caps it with a **local execution ceiling** — the effective mode is the more restrictive of the two, so a device can narrow a central grant but never widen it.

Both the **AI diagnosis panel** and the **terminal AI copilot** share one sealed confirmation chain. The copilot itself stays suggest-only — it never executes anything on its own. For a `confirm_required` suggestion, the operator may explicitly **promote it to execution**: that click relays the exact command to the host, which **re-classifies it server-side** (a control plane's self-reported decision is never trusted), mints the `exec_request_id`, and returns a preview the operator must **approve** before anything runs. Execution is gated by the same **local execution ceiling** — left at suggest-only it is off, so the Run action returns a non-executable preview that guides the owner to raise the ceiling first. Raising it opens confirmed execution for every AI surface on that device (diagnosis and copilot alike), not the copilot alone.

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
