//! Whitelist command templates and shared deterministic shell renderers.
//!
//! Each template is a fixed token pattern (literals + typed parameter slots)
//! plus a renderer that produces the canonical `(program, argv)`. Matching is on
//! tokens (see [`super::tokenize`]); a slot value is bound only after passing its
//! type validator, and the renderer emits argv directly — for PowerShell into a
//! fixed `-Command` template, never by concatenating the user's string. Because
//! the tokenizer already restricts characters to `[A-Za-z0-9 ._-]` and slot
//! validators tighten that per type, a bound value can never carry a shell
//! metacharacter, so quoting it into a `-Command` string cannot break out.
//!
//! The table is intentionally small and hard-coded; operator-configurable
//! templates are a later (policy-engine) concern.

use desk_agent_protocol::RiskLevel;
use desk_agent_protocol::exec::{ExecEffect, ExecShellKind};

/// A typed parameter slot. The validator runs on an already-tokenized value
/// (so the character set is the tokenizer's safe set); it tightens that to the
/// slot's type and rejects anything that could be read as a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    ServiceName,
    ProcessName,
    Pid,
    ContainerId,
    Port,
}

impl SlotKind {
    /// Angle-bracket placeholder name used when advertising a template's form to
    /// the model (e.g. `Get-Service -Name <service>`).
    fn placeholder(self) -> &'static str {
        match self {
            SlotKind::ServiceName => "service",
            SlotKind::ProcessName => "process",
            SlotKind::Pid => "pid",
            SlotKind::ContainerId => "container",
            SlotKind::Port => "port",
        }
    }

    fn validate(self, value: &str) -> bool {
        if value.is_empty() || value.starts_with('-') {
            return false;
        }
        match self {
            SlotKind::ServiceName | SlotKind::ProcessName => {
                value.len() <= 256
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            }
            SlotKind::ContainerId => {
                value.len() <= 128
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            }
            SlotKind::Pid => {
                value.len() <= 9
                    && value.chars().all(|c| c.is_ascii_digit())
                    && value.parse::<u32>().is_ok_and(|n| n > 0)
            }
            SlotKind::Port => value.parse::<u16>().is_ok_and(|n| n >= 1),
        }
    }
}

/// One element of a template's token pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matcher {
    /// Exact token, compared case-insensitively (PowerShell cmdlets and the
    /// `docker` subcommands are case-insensitive).
    Lit(&'static str),
    /// A parameter slot bound from the matching token.
    Slot(SlotKind),
}

/// A whitelist template: how to recognize a command and how to render it.
pub struct Template {
    pub id: &'static str,
    pub pattern: &'static [Matcher],
    pub shell: ExecShellKind,
    pub risk: RiskLevel,
    pub effect: ExecEffect,
    /// Human-readable description of what the template does.
    pub impact: &'static str,
    /// Render the canonical `(program, argv)` from the bound slot values, in
    /// pattern order. Values are pre-validated by their [`SlotKind`].
    pub render: fn(&[String]) -> (String, Vec<String>),
}

// ---- renderers ----

/// Wrap a fixed PowerShell command line as a non-interactive `-Command`
/// invocation. The command line is built only from literal text and
/// pre-validated values, so it carries no shell metacharacters.
pub(super) fn powershell_command(program: &str, command: String) -> (String, Vec<String>) {
    (
        program.to_string(),
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            command,
        ],
    )
}

fn powershell(command: String) -> (String, Vec<String>) {
    powershell_command("powershell", command)
}

fn render_get_service(b: &[String]) -> (String, Vec<String>) {
    powershell(format!("Get-Service -Name '{}'", b[0]))
}

fn render_get_process(b: &[String]) -> (String, Vec<String>) {
    powershell(format!("Get-Process -Name '{}'", b[0]))
}

fn render_get_nettcp(b: &[String]) -> (String, Vec<String>) {
    // Port is validated numeric; no quoting needed.
    powershell(format!("Get-NetTCPConnection -LocalPort {}", b[0]))
}

fn render_restart_service(b: &[String]) -> (String, Vec<String>) {
    powershell(format!("Restart-Service -Name '{}'", b[0]))
}

fn render_stop_process(b: &[String]) -> (String, Vec<String>) {
    powershell(format!("Stop-Process -Id {}", b[0]))
}

fn render_docker(args: &[&str]) -> (String, Vec<String>) {
    (
        "docker".to_string(),
        args.iter().map(|s| s.to_string()).collect(),
    )
}

fn render_docker_ps(_b: &[String]) -> (String, Vec<String>) {
    render_docker(&["ps"])
}

fn render_docker_logs(b: &[String]) -> (String, Vec<String>) {
    (
        "docker".to_string(),
        vec![
            "logs".to_string(),
            "--tail".to_string(),
            "200".to_string(),
            b[0].clone(),
        ],
    )
}

fn render_docker_inspect(b: &[String]) -> (String, Vec<String>) {
    (
        "docker".to_string(),
        vec!["inspect".to_string(), b[0].clone()],
    )
}

fn render_docker_restart(b: &[String]) -> (String, Vec<String>) {
    (
        "docker".to_string(),
        vec!["restart".to_string(), b[0].clone()],
    )
}

use Matcher::{Lit, Slot};
use SlotKind::{ContainerId, Pid, Port, ProcessName, ServiceName};

/// The whitelist table. Ordered so that more specific (named-flag) patterns are
/// tried before positional ones; matching takes the first full match.
pub fn templates() -> Vec<Template> {
    vec![
        // ---- read-only ----
        Template {
            id: "get_service_named",
            pattern: &[Lit("Get-Service"), Lit("-Name"), Slot(ServiceName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read the status of a Windows service",
            render: render_get_service,
        },
        Template {
            id: "get_service_positional",
            pattern: &[Lit("Get-Service"), Slot(ServiceName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read the status of a Windows service",
            render: render_get_service,
        },
        Template {
            id: "get_process_named",
            pattern: &[Lit("Get-Process"), Lit("-Name"), Slot(ProcessName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read information about a process",
            render: render_get_process,
        },
        Template {
            id: "get_process_positional",
            pattern: &[Lit("Get-Process"), Slot(ProcessName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read information about a process",
            render: render_get_process,
        },
        Template {
            id: "get_nettcp_port",
            pattern: &[Lit("Get-NetTCPConnection"), Lit("-LocalPort"), Slot(Port)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read TCP connections on a local port",
            render: render_get_nettcp,
        },
        Template {
            id: "docker_ps",
            pattern: &[Lit("docker"), Lit("ps")],
            shell: ExecShellKind::Native,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "List Docker containers",
            render: render_docker_ps,
        },
        Template {
            id: "docker_logs",
            pattern: &[Lit("docker"), Lit("logs"), Slot(ContainerId)],
            shell: ExecShellKind::Native,
            risk: RiskLevel::Medium,
            effect: ExecEffect::ReadOnly,
            impact: "Read recent logs from a Docker container",
            render: render_docker_logs,
        },
        Template {
            id: "docker_inspect",
            pattern: &[Lit("docker"), Lit("inspect"), Slot(ContainerId)],
            shell: ExecShellKind::Native,
            risk: RiskLevel::Low,
            effect: ExecEffect::ReadOnly,
            impact: "Read the configuration of a Docker container",
            render: render_docker_inspect,
        },
        // ---- mutating (High) ----
        Template {
            id: "restart_service_named",
            pattern: &[Lit("Restart-Service"), Lit("-Name"), Slot(ServiceName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::High,
            effect: ExecEffect::Mutating,
            impact: "Restart a Windows service",
            render: render_restart_service,
        },
        Template {
            id: "restart_service_positional",
            pattern: &[Lit("Restart-Service"), Slot(ServiceName)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::High,
            effect: ExecEffect::Mutating,
            impact: "Restart a Windows service",
            render: render_restart_service,
        },
        Template {
            id: "stop_process_id",
            pattern: &[Lit("Stop-Process"), Lit("-Id"), Slot(Pid)],
            shell: ExecShellKind::Powershell,
            risk: RiskLevel::High,
            effect: ExecEffect::Mutating,
            impact: "Terminate a process by PID",
            render: render_stop_process,
        },
        Template {
            id: "docker_restart",
            pattern: &[Lit("docker"), Lit("restart"), Slot(ContainerId)],
            shell: ExecShellKind::Native,
            risk: RiskLevel::High,
            effect: ExecEffect::Mutating,
            impact: "Restart a Docker container",
            render: render_docker_restart,
        },
    ]
}

/// One executable command form advertised to the diagnose model so it can
/// suggest commands the user can actually run (after explicit approval).
pub struct CommandForm {
    /// The canonical form with `<placeholder>` slots, e.g.
    /// `Get-Service -Name <service>`.
    pub form: String,
    /// Human-readable description of what the command does.
    pub impact: &'static str,
    /// Whether running it changes state (a High-risk template).
    pub mutating: bool,
}

/// Render a template's token pattern into a human-readable form (literals kept,
/// slots shown as `<placeholder>`).
fn pattern_form(pattern: &[Matcher]) -> String {
    pattern
        .iter()
        .map(|m| match m {
            Matcher::Lit(s) => (*s).to_string(),
            Matcher::Slot(kind) => format!("<{}>", kind.placeholder()),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The executable command catalog as advertised to the model, filtered to what
/// the active execution scope permits: read-only forms are always included;
/// `include_mutating` adds the state-changing (High) forms. Deduplicated by
/// rendered form so the named/positional variants of one cmdlet collapse.
pub fn command_forms(include_mutating: bool) -> Vec<CommandForm> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<CommandForm> = Vec::new();
    for t in templates() {
        let mutating = t.effect == ExecEffect::Mutating;
        if mutating && !include_mutating {
            continue;
        }
        let form = pattern_form(t.pattern);
        if seen.contains(&form) {
            continue;
        }
        seen.push(form.clone());
        out.push(CommandForm {
            form,
            impact: t.impact,
            mutating,
        });
    }
    out
}

/// A successful template match: the matched template and the bound slot values
/// (in pattern order).
pub struct TemplateMatch<'a> {
    pub template: &'a Template,
    pub bound: Vec<String>,
}

/// Try each template against the tokens; return the first full match (all
/// literals equal case-insensitively and all slots validate).
pub fn match_template<'a>(
    templates: &'a [Template],
    tokens: &[String],
) -> Option<TemplateMatch<'a>> {
    for template in templates {
        if let Some(bound) = match_one(template, tokens) {
            return Some(TemplateMatch { template, bound });
        }
    }
    None
}

fn match_one(template: &Template, tokens: &[String]) -> Option<Vec<String>> {
    if template.pattern.len() != tokens.len() {
        return None;
    }
    let mut bound = Vec::new();
    for (matcher, token) in template.pattern.iter().zip(tokens) {
        match matcher {
            Matcher::Lit(lit) => {
                if !token.eq_ignore_ascii_case(lit) {
                    return None;
                }
            }
            Matcher::Slot(kind) => {
                if !kind.validate(token) {
                    return None;
                }
                bound.push(token.clone());
            }
        }
    }
    Some(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        s.split(' ').map(|t| t.to_string()).collect()
    }

    #[test]
    fn matches_named_and_positional_forms() {
        let t = templates();
        let m = match_template(&t, &toks("Get-Service -Name Spooler")).unwrap();
        assert_eq!(m.template.id, "get_service_named");
        assert_eq!(m.bound, vec!["Spooler"]);

        let m = match_template(&t, &toks("Get-Service Spooler")).unwrap();
        assert_eq!(m.template.id, "get_service_positional");
    }

    #[test]
    fn literal_match_is_case_insensitive() {
        let t = templates();
        let m = match_template(&t, &toks("get-service spooler")).unwrap();
        assert_eq!(m.template.id, "get_service_positional");
    }

    #[test]
    fn renders_powershell_into_fixed_command() {
        let t = templates();
        let m = match_template(&t, &toks("Get-Service -Name Spooler")).unwrap();
        let (program, argv) = (m.template.render)(&m.bound);
        assert_eq!(program, "powershell");
        assert_eq!(
            argv,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Service -Name 'Spooler'"
            ]
        );
    }

    #[test]
    fn renders_docker_as_direct_argv() {
        let t = templates();
        let m = match_template(&t, &toks("docker logs web1")).unwrap();
        let (program, argv) = (m.template.render)(&m.bound);
        assert_eq!(program, "docker");
        assert_eq!(argv, vec!["logs", "--tail", "200", "web1"]);
    }

    #[test]
    fn mutating_templates_carry_high_risk_and_effect() {
        let t = templates();
        for (cmd, id) in [
            ("Restart-Service -Name Spooler", "restart_service_named"),
            ("Stop-Process -Id 1234", "stop_process_id"),
            ("docker restart web1", "docker_restart"),
        ] {
            let m = match_template(&t, &toks(cmd)).unwrap();
            assert_eq!(m.template.id, id);
            assert_eq!(m.template.risk, RiskLevel::High);
            assert_eq!(m.template.effect, ExecEffect::Mutating);
        }
    }

    #[test]
    fn slot_validators_reject_bad_values() {
        assert!(!SlotKind::Pid.validate("0"));
        assert!(!SlotKind::Pid.validate("12a"));
        assert!(SlotKind::Pid.validate("1234"));
        assert!(!SlotKind::Port.validate("70000"));
        assert!(SlotKind::Port.validate("8080"));
        assert!(!SlotKind::ServiceName.validate("-Name"));
        assert!(!SlotKind::ContainerId.validate("a.b")); // '.' not allowed in ids
        assert!(SlotKind::ContainerId.validate("a-b_c"));
    }

    #[test]
    fn command_forms_render_and_filter_by_mutating() {
        // Read-only only: no mutating verbs, slots rendered as placeholders.
        let ro = command_forms(false);
        assert!(ro.iter().all(|c| !c.mutating));
        assert!(ro.iter().any(|c| c.form == "Get-Service -Name <service>"));
        assert!(ro.iter().any(|c| c.form == "docker logs <container>"));
        assert!(!ro.iter().any(|c| c.form.starts_with("Restart-Service")));

        // Including mutating adds the state-changing forms.
        let all = command_forms(true);
        assert!(
            all.iter()
                .any(|c| c.mutating && c.form == "Restart-Service -Name <service>")
        );
        assert!(all.iter().any(|c| c.form == "Stop-Process -Id <pid>"));
        // A roundtrip of a rendered form (placeholder filled) matches its template.
        let table = templates();
        assert!(match_template(&table, &toks("Get-Service -Name Spooler")).is_some());
    }

    #[test]
    fn no_match_for_unknown_or_malformed() {
        let t = templates();
        assert!(match_template(&t, &toks("Remove-Item C")).is_none());
        assert!(match_template(&t, &toks("Get-Service")).is_none()); // needs a target
        assert!(match_template(&t, &toks("Stop-Process -Id abc")).is_none());
    }
}
