//! Confirmed-execution wire types (control end ↔ server) and the daemon's
//! authoritative execution plan.
//!
//! These shapes define the controlled suggest → confirm → execute → backfill
//! loop shared by the risk classifier, confirm-flow state machine, worker
//! executor, and signaling layer.
//!
//! Two safety invariants are baked into the types:
//!
//! - **The worker never re-parses a command string.** The daemon classifies the
//!   request, renders a frozen [`ExecPlan`] (program + argv, bound parameters,
//!   no shell metacharacters), and the worker executes that argv verbatim. The
//!   control end's free-form [`crate::ExecInput::command`] is used only for
//!   classification and preview — it is **never** sent to the worker.
//! - **Execution basis is explicit.** Template execution remains the default.
//!   A trusted central may authorize an authenticated device owner to approve
//!   one off-template command, which is marked
//!   [`ExecExecutionBasis::OwnerBlocklistOnly`]. Blocklist hits remain
//!   hard-denied, and every real execution still requires an explicit user
//!   approval that mints an `approval_id` server-side.
//!
//! All wire types derive `serde` (JSON, control-end signaling), `wincode` (the
//! daemon ↔ worker IPC carrying [`ExecPlan`] / [`ExecResultPayload`]), and
//! `utoipa::ToSchema`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

use crate::{AgentOperation, AgentOutcome, RiskLevel};

// ============================ Correlation IDs ============================

/// Server-minted primary correlation key for one confirmed-execution attempt.
/// Threads `ExecPreview` → `ResolveExec` → `ExecResult` and lets the UI backfill
/// the result onto the right suggested-command row. Serializes transparently as
/// the inner string.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(transparent)]
pub struct ExecRequestId(pub String);

/// Server-minted anti-forgery approval token. Created **only** when the user
/// approves an execution; a control-end / model-supplied value is ignored.
/// Flows into [`ExecPlan`] and the `ai.approval.granted` audit event.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(transparent)]
pub struct ApprovalId(pub String);

// ============================ Classification ============================

/// What a server-classified command does to the device. Drives the
/// `shell.exec.readonly` vs `shell.exec.confirmed` capability split (see
/// [`crate::OperationInput::required_capability`]). Only meaningful for a
/// template-matched (executable) classification.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecEffect {
    /// Reads state only (e.g. `Get-Service`, `docker logs`).
    ReadOnly,
    /// Changes state (e.g. `Restart-Service`, `Stop-Process`).
    Mutating,
}

/// The server's executability decision for a classified command.
///
/// Three outcomes, all strictly non-automatic — even `ConfirmRequired` still
/// needs an explicit user approval before anything runs. `NotExecutable` is
/// *more* restrictive than execution, not a relaxation.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecDecision {
    /// A whitelist template matched. Executable, but only after an explicit user
    /// approval mints an `approval_id`.
    ConfirmRequired,
    /// No whitelist template matched. Previewed for the operator but not
    /// executable through the AI path — the UI falls back to suggest-only.
    NotExecutable,
    /// A blocklist rule matched. Hard-denied; never executable.
    Blocked,
}

/// The result of classifying a command. Server-internal (produced by the risk
/// classifier, consumed by the confirm flow), but a wire type so it can be
/// recorded / round-tripped consistently.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct CommandClassification {
    pub risk: RiskLevel,
    /// Identifier of the matched whitelist template, or `None` for a blocklist
    /// hit / off-template command.
    pub matched_template: Option<String>,
    /// Human-readable description of what executing this would do.
    pub impact: String,
    pub decision: ExecDecision,
    /// `Some` only for a template-matched (executable) classification; `None`
    /// for `Blocked` / `NotExecutable`.
    pub effect: Option<ExecEffect>,
}

// ============================ Confirm-flow DTOs ============================

/// Control end → server (`ConfirmExec`): request a preview/classification of an
/// exec operation. Carries only the non-authoritative parts (mirrors
/// [`crate::AgentRequestData`]); it has **no id field** — the server mints the
/// [`ExecRequestId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ConfirmExecData {
    /// The exec operation to classify (`operation.input` is
    /// [`crate::OperationInput::Exec`]).
    pub operation: AgentOperation,
    /// Free-text "why" from the control end; flows into the audit event.
    pub reason: Option<String>,
    /// Manager-only org context hint: the id of the organization the operator is
    /// acting within. NON-authoritative — the manager validates the operator's
    /// membership in this org AND the org's device-access grant to the target
    /// device before trusting it, then adjudicates the exec against that single
    /// org's policy and command templates. The open-source single-instance
    /// desk-server has no org concept and **ignores** this field; `None` (the
    /// default, sent by every non-manager client) is the personal view.
    #[serde(default)]
    pub org_id: Option<i32>,
}

/// Server → control end (`ExecPreview`): the classification result the
/// confirmation dialog renders.
///
/// `exec_request_id` is `Some` only when execution can proceed (immediately or
/// after approval); denied commands carry no request id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ExecPreview {
    pub exec_request_id: Option<ExecRequestId>,
    pub shell: String,
    /// The original command string, for operator display only (the worker never
    /// receives it).
    pub command: String,
    pub cwd: Option<String>,
    /// How long this approval remains resolvable. This is independent of
    /// `timeout_ms`, which limits the command after it starts running.
    pub approval_timeout_ms: u64,
    pub timeout_ms: u32,
    pub risk: RiskLevel,
    /// The server-authoritative basis used to classify this preview.
    #[serde(default)]
    pub execution_basis: ExecExecutionBasis,
    pub requires_confirmation: bool,
    /// Whether this command can be executed through the AI path at all.
    pub executable: bool,
    /// The operator-facing reason for a non-executable preview.
    pub blocked_reason: Option<String>,
}

/// The user's decision on a previewed execution.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Reject,
}

/// Control end → server (`ResolveExec`): approve or reject a previewed
/// execution, referenced by its server-minted [`ExecRequestId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ResolveExecData {
    pub exec_request_id: ExecRequestId,
    pub decision: ApprovalDecision,
}

/// Server → control end (`ExecResult`) **and** worker → daemon
/// (`WorkerToService::ExecutionCompleted`): the execution result, tagged with its
/// [`ExecRequestId`] so the UI backfills the right suggested-command row.
///
/// Carries [`AgentOutcome`] (`Ok(OperationOutput::Exec(..))` | `Err`) rather
/// than reusing `AgentResponse`, which has no `exec_request_id` and rides the
/// separate raw-read path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema)]
pub struct ExecResultPayload {
    pub exec_request_id: ExecRequestId,
    pub outcome: AgentOutcome,
}

// ============================ Authoritative plan ============================

/// Shell family a rendered command belongs to. Informational (for audit /
/// preview) — the worker always executes [`ExecPlan::program`] + argv directly,
/// never wrapping in `cmd /c` / `bash -c`. `Native` is a direct executable with
/// no shell interpreter (e.g. `docker`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecShellKind {
    Native,
    Powershell,
    Bash,
    Sh,
}

/// The trust basis under which an execution draft was produced.
///
/// This is classification metadata. It is compared as part of the full draft
/// but deliberately excluded from the worker-field fingerprint.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecExecutionBasis {
    /// The command matched a built-in or operator template.
    #[default]
    Template,
    /// The device owner explicitly approved an off-template command after the
    /// effective blocklist and structural checks passed.
    OwnerBlocklistOnly,
}

/// The OS-level containment tier a template demands. The edge fails closed
/// **before spawn** if it cannot meet the tier — it is never a silent downgrade.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RequiredEnforcement {
    /// Bounded wall time + process-tree recycle + concurrency cap. Every platform
    /// must satisfy this; it is the safe default.
    #[default]
    Baseline,
    /// Baseline plus aggregate CPU / memory / process-count hard limits (Linux
    /// cgroup v2, Windows Job Object). A device that cannot enforce it rejects the
    /// plan before spawning.
    NativeHard,
}

impl RequiredEnforcement {
    /// A stable discriminant byte for the plan fingerprint. Kept explicit (not a
    /// cast) so reordering the variants can never silently shift a fingerprint.
    pub fn fingerprint_byte(self) -> u8 {
        match self {
            RequiredEnforcement::Baseline => 0,
            RequiredEnforcement::NativeHard => 1,
        }
    }
}

/// The immutable containment declaration bound into an approved plan and shown at
/// approval time. It carries the resource / governance envelope; the **wall-time
/// limit is deliberately not here** — that is the plan's [`ExecPlanDraft::timeout_ms`]
/// (the single source read by the worker, the capacity gate, and the manager wait),
/// so it can never diverge between two copies.
///
/// The aggregate hard-limit fields (`max_processes` … `io_max_bytes_per_sec`) are
/// only meaningful under [`RequiredEnforcement::NativeHard`]; a `Baseline` template
/// leaves them `None` so an approval can never display a limit the edge silently
/// ignores. They are declared and fingerprinted now; wiring them to the native
/// backend (cgroup / Job Object) is a follow-on.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecContainmentSnapshot {
    /// Whether the command may run past the foreground wall-time ceiling. Only a
    /// template on the long-running whitelist sets this; it gates whether the
    /// product background cap (rather than the foreground cap) applies.
    pub allow_background: bool,
    /// OS enforcement tier the template demands.
    pub required_enforcement: RequiredEnforcement,
    /// Aggregate hard caps, all `None` unless the template is `NativeHard`.
    pub max_processes: Option<u32>,
    pub max_memory_bytes: Option<u64>,
    pub cpu_max_percent: Option<u16>,
    pub io_max_bytes_per_sec: Option<u64>,
    /// The named resource profile this snapshot was resolved from, and that
    /// profile's revision, so the approval binding is reproducible.
    pub resource_profile_id: Option<String>,
    pub resource_profile_revision: Option<i64>,
}

/// The daemon's authoritative, renderable-but-not-yet-approved execution plan.
///
/// Built at `ConfirmExec` from the matched template + validated parameters and
/// stored **immutably** in the pending-approval store, keyed by its
/// [`ExecRequestId`] (which is *not* a field here — it is the store key). Holds
/// every field needed to execute **except** `approval_id`, which is minted only
/// when the user approves. Sealing the draft at preview time removes the
/// "previewed ≠ executed" gap: `ResolveExec` does not re-render.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecPlanDraft {
    /// Program to execute (the actual executable, e.g. `docker`, `powershell`).
    pub program: String,
    /// Arguments, already bound from validated template slots. Executed
    /// verbatim — never re-parsed or shell-expanded.
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub shell: ExecShellKind,
    pub risk: RiskLevel,
    /// The classification basis. This is not part of `fingerprint`; callers
    /// reject drift by comparing the complete draft.
    #[serde(default)]
    pub execution_basis: ExecExecutionBasis,
    /// Identifier of the whitelist template this was rendered from.
    pub template_id: String,
    /// Stable hash over `program + argv + cwd + limits` (PowerShell templates
    /// fold their fixed `-Command` render in). Detects any tampering between
    /// preview and execution.
    pub fingerprint: String,
    pub timeout_ms: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
    /// The resource / governance envelope this plan runs under (wall time excepted;
    /// that is `timeout_ms`). Bound at seal time and fingerprinted.
    #[serde(default)]
    pub containment: ExecContainmentSnapshot,
}

/// The sealed, approved execution plan sent to the worker
/// (`ServiceToWorker::ExecPlan`). An [`ExecPlanDraft`] plus the server-minted
/// [`ExecRequestId`] and [`ApprovalId`]. The worker executes `program` + `argv`
/// verbatim and never sees the original command string.
///
/// # Two identity axes
///
/// A plan carries both, and they answer different questions:
///
/// - [`exec_request_id`](Self::exec_request_id) — the **task**: the piece of work
///   an operator asked for. Stable across retries of that work.
/// - [`execution_generation`](Self::execution_generation) — this **one dispatch**
///   of it. A retry is a new generation of the same task.
///
/// The distinction is what lets a host say "I already ran this exact dispatch"
/// while still allowing a genuine retry: deduplicating on the task would block
/// legitimate retries, and deduplicating on nothing would let a redelivered frame
/// run a command twice. Callers must supply both explicitly — never derive one
/// from the other.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct ExecPlan {
    /// The task axis: stable across retries, and what a control end correlates a
    /// result to for display.
    pub exec_request_id: ExecRequestId,
    /// The dispatch axis: unique per send, and what the host deduplicates on. It
    /// equals the signalling frame's own `request_id`, which is both the worker's
    /// correlation key and naturally distinct per delivery.
    pub execution_generation: String,
    pub program: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub shell: ExecShellKind,
    pub risk: RiskLevel,
    /// Copied verbatim from the approved draft.
    #[serde(default)]
    pub execution_basis: ExecExecutionBasis,
    pub template_id: String,
    /// Minted at approval time; proves the execution was user-approved.
    pub approval_id: ApprovalId,
    pub fingerprint: String,
    pub timeout_ms: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
    /// The resource / governance envelope this plan runs under (wall time excepted;
    /// that is `timeout_ms`). Copied verbatim from the sealed draft.
    #[serde(default)]
    pub containment: ExecContainmentSnapshot,
}

impl ExecPlan {
    /// Seal a stored [`ExecPlanDraft`] into an approved plan with the
    /// server-minted ids. The single place a draft becomes executable — keeps
    /// the field copy in one spot so the draft can never silently diverge from
    /// the plan.
    ///
    /// Both identity axes are parameters rather than derived, so every call site
    /// has to state which id plays which role. Their meanings have differed
    /// between dispatch paths before; making the compiler ask is the point.
    pub fn from_draft(
        exec_request_id: ExecRequestId,
        execution_generation: impl Into<String>,
        approval_id: ApprovalId,
        draft: ExecPlanDraft,
    ) -> Self {
        ExecPlan {
            exec_request_id,
            execution_generation: execution_generation.into(),
            program: draft.program,
            argv: draft.argv,
            cwd: draft.cwd,
            shell: draft.shell,
            risk: draft.risk,
            execution_basis: draft.execution_basis,
            template_id: draft.template_id,
            approval_id,
            fingerprint: draft.fingerprint,
            timeout_ms: draft.timeout_ms,
            max_stdout_bytes: draft.max_stdout_bytes,
            max_stderr_bytes: draft.max_stderr_bytes,
            containment: draft.containment,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentError, AgentErrorKind, ExecOutput, OperationInput, OperationOutput};
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    fn readonly_classification() -> CommandClassification {
        CommandClassification {
            risk: RiskLevel::Low,
            matched_template: Some("get_service".into()),
            impact: "Reads the status of a service".into(),
            decision: ExecDecision::ConfirmRequired,
            effect: Some(ExecEffect::ReadOnly),
        }
    }

    fn mutating_classification() -> CommandClassification {
        CommandClassification {
            risk: RiskLevel::High,
            matched_template: Some("restart_service".into()),
            impact: "Restarts a service".into(),
            decision: ExecDecision::ConfirmRequired,
            effect: Some(ExecEffect::Mutating),
        }
    }

    fn sample_draft() -> ExecPlanDraft {
        ExecPlanDraft {
            program: "powershell".into(),
            argv: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Get-Service -Name 'Spooler'".into(),
            ],
            cwd: None,
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            execution_basis: ExecExecutionBasis::Template,
            template_id: "get_service".into(),
            fingerprint: "abc123".into(),
            timeout_ms: 10_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
            containment: ExecContainmentSnapshot::default(),
        }
    }

    #[test]
    fn required_capability_maps_template_effect() {
        assert_eq!(
            OperationInput::required_capability(&readonly_classification()),
            Some(crate::Capability::ShellExecReadonly)
        );
        assert_eq!(
            OperationInput::required_capability(&mutating_classification()),
            Some(crate::Capability::ShellExecConfirmed)
        );
    }

    #[test]
    fn required_capability_is_none_for_non_executable() {
        for decision in [ExecDecision::Blocked, ExecDecision::NotExecutable] {
            let c = CommandClassification {
                risk: RiskLevel::High,
                matched_template: None,
                impact: "n/a".into(),
                decision,
                effect: None,
            };
            assert_eq!(OperationInput::required_capability(&c), None);
        }
        // Defensive: even a stray effect cannot make a non-confirm decision
        // executable.
        let c = CommandClassification {
            risk: RiskLevel::High,
            matched_template: None,
            impact: "n/a".into(),
            decision: ExecDecision::Blocked,
            effect: Some(ExecEffect::ReadOnly),
        };
        assert_eq!(OperationInput::required_capability(&c), None);
    }

    #[test]
    fn exec_decision_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExecDecision::ConfirmRequired).unwrap(),
            "\"confirm_required\""
        );
        assert_eq!(
            serde_json::to_string(&ExecDecision::NotExecutable).unwrap(),
            "\"not_executable\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::Approve).unwrap(),
            "\"approve\""
        );
    }

    #[test]
    fn from_draft_seals_ids_without_changing_plan_fields() {
        let draft = sample_draft();
        let plan = ExecPlan::from_draft(
            ExecRequestId("exec_1".into()),
            "gen_1",
            ApprovalId("appr_1".into()),
            draft.clone(),
        );
        assert_eq!(plan.exec_request_id, ExecRequestId("exec_1".into()));
        assert_eq!(plan.execution_generation, "gen_1");
        assert_eq!(plan.approval_id, ApprovalId("appr_1".into()));
        assert_eq!(plan.program, draft.program);
        assert_eq!(plan.argv, draft.argv);
        assert_eq!(plan.fingerprint, draft.fingerprint);
        assert_eq!(plan.template_id, draft.template_id);
        assert_eq!(plan.timeout_ms, draft.timeout_ms);
        assert_eq!(plan.execution_basis, draft.execution_basis);
    }

    /// Retrying a task keeps its task id and takes a fresh generation. The two
    /// axes must stay independent — collapsing them would either block retries or
    /// let a redelivered dispatch run twice.
    #[test]
    fn a_retry_keeps_the_task_and_takes_a_new_generation() {
        let draft = sample_draft();
        let first = ExecPlan::from_draft(
            ExecRequestId("exec_1".into()),
            "gen_1",
            ApprovalId("appr_1".into()),
            draft.clone(),
        );
        let retry = ExecPlan::from_draft(
            ExecRequestId("exec_1".into()),
            "gen_2",
            ApprovalId("appr_1".into()),
            draft,
        );
        assert_eq!(first.exec_request_id, retry.exec_request_id);
        assert_ne!(first.execution_generation, retry.execution_generation);
    }

    #[test]
    fn confirm_flow_dtos_round_trip_json_and_wincode() {
        let config = unbounded_config();

        let preview = ExecPreview {
            exec_request_id: Some(ExecRequestId("exec_1".into())),
            shell: "powershell".into(),
            command: "Get-Service -Name Spooler".into(),
            cwd: None,
            approval_timeout_ms: 120_000,
            timeout_ms: 10_000,
            risk: RiskLevel::Low,
            execution_basis: ExecExecutionBasis::Template,
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        let resolve = ResolveExecData {
            exec_request_id: ExecRequestId("exec_1".into()),
            decision: ApprovalDecision::Approve,
        };
        let result_ok = ExecResultPayload {
            exec_request_id: ExecRequestId("exec_1".into()),
            outcome: AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                exit_code: 0,
                stdout: "Running".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: 12,
                redactions: vec![],
            })),
        };
        let result_err = ExecResultPayload {
            exec_request_id: ExecRequestId("exec_2".into()),
            outcome: AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::Timeout,
                message: "timed out".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        };

        // JSON round-trips. The preview carries only fields needed to render and
        // resolve execution; classifier prose is not part of the control protocol.
        let preview_json = serde_json::to_string(&preview).unwrap();
        assert!(!preview_json.contains("\"impact\""));
        assert!(!preview_json.contains("\"policy_note\""));
        for json in [
            preview_json,
            serde_json::to_string(&resolve).unwrap(),
            serde_json::to_string(&result_ok).unwrap(),
            serde_json::to_string(&result_err).unwrap(),
        ] {
            assert!(!json.is_empty());
        }
        assert_eq!(
            serde_json::from_str::<ExecPreview>(&serde_json::to_string(&preview).unwrap()).unwrap(),
            preview
        );
        assert_eq!(
            serde_json::from_str::<ResolveExecData>(&serde_json::to_string(&resolve).unwrap())
                .unwrap(),
            resolve
        );

        // wincode round-trips (these cross the daemon ↔ worker IPC).
        for payload in [result_ok, result_err] {
            let bytes = wincode::config::serialize(&payload, config).expect("encode");
            let back: ExecResultPayload =
                wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(payload, back);
        }
    }

    #[test]
    fn legacy_json_defaults_execution_basis_to_template() {
        let mut draft_value = serde_json::to_value(sample_draft()).expect("encode draft");
        draft_value
            .as_object_mut()
            .expect("draft object")
            .remove("execution_basis");
        let draft: ExecPlanDraft = serde_json::from_value(draft_value).expect("decode draft");
        assert_eq!(draft.execution_basis, ExecExecutionBasis::Template);

        let plan = ExecPlan::from_draft(
            ExecRequestId("exec_legacy".into()),
            "gen_legacy",
            ApprovalId("approval_legacy".into()),
            sample_draft(),
        );
        let mut plan_value = serde_json::to_value(plan).expect("encode plan");
        plan_value
            .as_object_mut()
            .expect("plan object")
            .remove("execution_basis");
        let plan: ExecPlan = serde_json::from_value(plan_value).expect("decode plan");
        assert_eq!(plan.execution_basis, ExecExecutionBasis::Template);
    }

    #[test]
    fn exec_plan_wincode_round_trips() {
        let config = unbounded_config();
        let plan = ExecPlan::from_draft(
            ExecRequestId("exec_1".into()),
            "gen_1",
            ApprovalId("appr_1".into()),
            sample_draft(),
        );
        let bytes = wincode::config::serialize(&plan, config).expect("encode");
        let back: ExecPlan = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(plan, back);
    }

    #[test]
    fn classification_round_trips() {
        let c = mutating_classification();
        let json = serde_json::to_string(&c).unwrap();
        let back: CommandClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn utoipa_schema_is_generated() {
        use utoipa::PartialSchema;
        let _ = ExecPreview::schema();
        let _ = ResolveExecData::schema();
        let _ = ExecResultPayload::schema();
        let _ = ExecPlan::schema();
        let _ = CommandClassification::schema();
    }
}
