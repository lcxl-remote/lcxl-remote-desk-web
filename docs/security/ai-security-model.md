# AI Security Model

Remote desktop with AI that reads system state is powerful — and demands a strong trust boundary. LCXL Remote Desk treats AI as a first-class control plane, governed by invariants that are **security-relevant: breaking them is a regression**.

## The Server Is the Sole Source of Truth

All authorization logic is verified **server-side**. Fields like `request_id`, `target`, `actor`, `scope`, `caller`, the final `risk`, and `approval_id` are injected and validated by the server — a control plane (browser, mobile, or MCP) can **never self-report** them. The browser-side request body does not even contain these fields structurally.

## Capability Protocol Is Device-Facing

The capability protocol describes **what can be done to a device**, independent of who is calling. Read-permission points are **derived from the input** (`OperationInput::capability()`), which prevents drift between capabilities, evidence collection, and audit.

## Suggest-Only by Default

The default execution mode is **suggest-only**: the model can propose commands but cannot execute them. Higher-risk actions require explicit, **server-mediated confirmation**.

## Redaction Fails Closed

The diagnostic orchestrator runs **collect → redact → model → render**. If redaction fails, the request is aborted **before** the model is called. Evidence is always redacted before it reaches the model.

## API Keys Are Server-Side Secrets

AI model API keys are **never** returned to the browser, **never** included in any public `/settings` DTO, and **never** written to logs.

## Audit Records Metadata Only

Audit trails log content-free summaries — counts, sizes, token usage, provider, adapter. Raw prompts, outputs, and screenshots are **never** persisted.

## MCP Is Read-Only

The MCP tool set is a **static whitelist** with no execute / write / control tools by construction ("undefined means unreachable"). The `lcxl_diagnose` provider signature carries no screenshot option, so MCP clients structurally cannot capture the screen. See the [MCP Server](/features/mcp-server) page.

## Model Agnostic

Wire protocols are isolated behind adapters (`openai.rs` for OpenAI-compatible, `anthropic.rs` for Anthropic Messages). Adding a provider means adding an adapter — the orchestrator does not change.
