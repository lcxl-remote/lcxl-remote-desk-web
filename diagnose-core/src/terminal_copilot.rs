//! Terminal AI copilot: model-agnostic prompt assembly, response parsing, and
//! the server-authoritative per-suggestion execution decision.
//!
//! Shared by the web portable runtime and the manager central orchestrator so
//! the two can never drift. Pure logic: it builds the prompt, parses the model's
//! final JSON answer, and — crucially — computes each suggestion's `risk` /
//! `decision` itself via the shared [`crate::exec_classify`] classifier. The
//! model's own output never carries those fields, so a prompt-injected model
//! cannot mark a command as safe to run.

use desk_agent_protocol::content_safety::StreamRetractionReason;
use desk_agent_protocol::provenance::AiProvenance;
use desk_agent_protocol::terminal_copilot::{
    CommandSuggestion, CopilotHistoryTurn, TerminalCopilotAnswer, TerminalCopilotAsk,
    TerminalCopilotEvent, TerminalCopilotMode,
};
use desk_agent_protocol::{AgentError, ExecInput, ExecTarget};
use serde::Deserialize;

use crate::chat::{ChatMessage, ChatRole};
use crate::exec_classify::classify_command;
use crate::parser::{ParseOutcome, extract_json_object, truncate_on_char_boundary};
use crate::read_tools::read_tool_registry;
use crate::registry::RegisteredTool;
use crate::seam::TurnSink;

/// Default per-turn reasoning-round budget for the interactive copilot.
///
/// The manager treats this as the fallback when its platform-configured limit
/// (`ai.terminal_copilot.max_steps_per_turn`) is absent or unparseable.
pub const COPILOT_MAX_STEPS_PER_TURN: u32 = crate::MAX_STEPS_PER_TURN;
/// Semantic version of the shared Terminal Copilot system prompt. Bump whenever
/// `build_copilot_system_message` changes.
pub const COPILOT_PROMPT_VERSION: &str = "copilot-v1";

/// Max bytes of recent terminal output forwarded to the model (after the runtime
/// has redacted it). Caps prompt size / latency; the runtime redacts first.
pub const MAX_RECENT_OUTPUT_BYTES: usize = 4_096;

/// Max prior turns the stateless path replays into the prompt (oldest dropped
/// past this). Bounds prompt growth across a long terminal conversation.
pub const MAX_COPILOT_HISTORY_TURNS: usize = 6;

/// Max bytes kept per replayed history message (user or assistant), truncated on a
/// char boundary. Bounds a single oversized prior turn.
pub const MAX_COPILOT_HISTORY_MSG_BYTES: usize = 2_000;

/// Degraded-answer cap: how much raw model text to keep as the explanation when
/// the structured JSON parse fails.
const MAX_DEGRADED_EXPLANATION_BYTES: usize = 4_000;

/// The read-only tools the copilot may call to gather evidence: a deliberately
/// small subset (system info + process list). For any fact outside these the
/// model is instructed to *suggest* a read-only diagnostic command rather than
/// run it.
pub fn copilot_read_tools() -> Vec<RegisteredTool> {
    const ALLOWED: [&str; 2] = ["read_system_info", "read_process_list"];
    read_tool_registry()
        .into_iter()
        .filter(|t| ALLOWED.contains(&t.spec.name.as_str()))
        .collect()
}

/// Build the copilot system prompt for `mode`. Not persisted in the conversation
/// (re-prepended on every model call, like the diagnose agentic prompt).
///
/// `locale` is the operator's UI language (BCP-47, e.g. `zh-CN`); when present it
/// steers the answer's natural-language text. It only governs prose — commands,
/// shell names, paths, and code identifiers stay verbatim.
pub fn build_copilot_system_message(
    mode: TerminalCopilotMode,
    locale: Option<&str>,
) -> ChatMessage {
    let task = match mode {
        TerminalCopilotMode::HowTo => {
            "The operator described what they want to do in their terminal. \
             Propose the command(s) that accomplish it."
        }
        TerminalCopilotMode::ExplainError => {
            "The operator hit a problem in their terminal. Diagnose the root cause, \
             then propose the command(s) that fix it."
        }
    };
    let language_rule = match locale.map(str::trim).filter(|l| !l.is_empty()) {
        Some(locale) => format!(
            "\n- Write all natural-language text (the explanation and each \
             suggestion's note) in the operator's UI language (BCP-47 `{locale}`). \
             Keep commands, shell names, paths, and code identifiers unchanged."
        ),
        None => String::new(),
    };
    let body = format!(
        "You are a terminal assistant embedded in a remote shell session.\n\
         {task}\n\n\
         Rules:\n\
         - You only advise. You never execute anything; the operator alone decides \
         whether to run a suggestion. Never claim a command has already been run.\n\
         - You may call the read-only tools (read_system_info, read_process_list) a \
         couple of times to check facts. If the fact you need is not available from \
         those tools, do NOT guess — propose a read-only diagnostic command as a \
         suggestion for the operator to run.\n\
         - Use the operator's OS and shell (given in the request). Keep commands \
         minimal; avoid destructive operations.{language_rule}\n\n\
         - Refuse to generate, transform, summarize, translate, role-play, or \
         operationalize sexual content (especially involving minors), violence or \
         graphic injury, violent wrongdoing, hate or threatening harassment, \
         self-harm instructions, or illicit real-world wrongdoing. Do not propose \
         commands that advance such requests.\n\
         - Refuse substantive content about political figures, parties, elections, \
         political systems, government policy, war positions, geopolitics, or \
         political movements, including factual explanation, evaluation, prediction, \
         persuasion, or propaganda. Allow political names, institutions, sites, or \
         words only as incidental technical objects in logs, files, processes, DNS, \
         TLS, networking, or security response, and keep the response strictly \
         technical. Computer terms such as leader election are not political content.\n\
         Final answer — emit exactly two parts, in this order:\n\
         1. Write your explanation as Markdown prose. This streams to the operator \
         as you write it, so the human-readable answer goes here, not inside the \
         JSON.\n\
         2. Then append the command suggestions as a SINGLE fenced ```json code \
         block, and output nothing after the closing fence:\n\
         ```json\n\
         {{\"suggestions\": [{{\"command\": \"...\", \"shell\": \"bash|pwsh|cmd|...\", \
         \"cwd\": null, \"note\": \"<one line: what it does>\"}}]}}\n\
         ```\n\
         Do not include risk or approval fields — the server computes those. \
         `suggestions` may be empty when no command is appropriate."
    );
    ChatMessage::text("copilot-system", ChatRole::System, body)
}

/// Build the user turn from the ask: the (non-authoritative) environment hints
/// plus the question or error passage, with recent output length-capped. The
/// runtime must redact `context` before calling this.
pub fn build_copilot_user_message(ask: &TerminalCopilotAsk) -> ChatMessage {
    let ctx = &ask.context;
    let mut body = String::new();
    body.push_str(&format!("OS: {}\nShell: {}\n", ctx.os, ctx.shell));
    if let Some(cwd) = &ctx.cwd {
        body.push_str(&format!("CWD: {cwd}\n"));
    }
    if let Some(last) = &ctx.last_command {
        body.push_str(&format!("Last command: {last}\n"));
    }
    match ask.mode {
        TerminalCopilotMode::HowTo => {
            let q = ask.question.as_deref().unwrap_or("").trim();
            body.push_str(&format!("\nRequest: {q}\n"));
        }
        TerminalCopilotMode::ExplainError => {
            if let Some(err) = ctx.error_text.as_deref() {
                let err = truncate_on_char_boundary(err.trim(), MAX_RECENT_OUTPUT_BYTES);
                body.push_str(&format!("\nError:\n{err}\n"));
            }
        }
    }
    let recent = truncate_on_char_boundary(ctx.recent_output.trim(), MAX_RECENT_OUTPUT_BYTES);
    if !recent.is_empty() {
        body.push_str(&format!("\nRecent terminal output:\n{recent}\n"));
    }
    ChatMessage::text("copilot-user", ChatRole::User, body)
}

/// Build the prior-conversation messages for a stateless multi-turn copilot turn:
/// the last [`MAX_COPILOT_HISTORY_TURNS`] turns mapped to alternating user /
/// assistant [`ChatMessage`]s, each length-capped to [`MAX_COPILOT_HISTORY_MSG_BYTES`].
/// Empty messages are skipped. The runtime must have redacted the history first;
/// this only caps and assembles it. The manager path never calls this — its DB
/// session is authoritative.
pub fn build_copilot_history_messages(history: &[CopilotHistoryTurn]) -> Vec<ChatMessage> {
    let start = history.len().saturating_sub(MAX_COPILOT_HISTORY_TURNS);
    let mut messages = Vec::new();
    for turn in &history[start..] {
        let user = truncate_on_char_boundary(turn.user.trim(), MAX_COPILOT_HISTORY_MSG_BYTES);
        if !user.is_empty() {
            messages.push(ChatMessage::text(
                "copilot-history-user",
                ChatRole::User,
                user,
            ));
        }
        let assistant =
            truncate_on_char_boundary(turn.assistant.trim(), MAX_COPILOT_HISTORY_MSG_BYTES);
        if !assistant.is_empty() {
            messages.push(ChatMessage::text(
                "copilot-history-assistant",
                ChatRole::Assistant,
                assistant,
            ));
        }
    }
    messages
}

/// The model's raw final answer, before the server stamps risk / decision. The
/// suggestion shape deliberately omits `risk` / `decision`, so a model cannot
/// self-report them — the classifier computes them in [`finalize_suggestion`].
#[derive(Deserialize)]
struct RawAnswer {
    #[serde(default)]
    explanation_md: String,
    #[serde(default)]
    suggestions: Vec<RawSuggestion>,
}

#[derive(Deserialize)]
struct RawSuggestion {
    command: String,
    #[serde(default)]
    shell: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    note: String,
}

/// Parse the model's final answer into a [`TerminalCopilotAnswer`], computing the
/// server-authoritative `risk` / `decision` for each suggestion via the shared
/// classifier.
///
/// The prompt asks the model to stream the explanation as Markdown prose and then
/// append the suggestions in a trailing ```json fenced block. So the preferred
/// shape is: the prose **before** the fence is the explanation (it is exactly what
/// streamed to the operator), and the fenced JSON yields the suggestions. A model
/// that ignores the fence and emits a bare JSON object still parses (legacy
/// fallback). When neither shape is present the answer degrades to explanation-only
/// (no suggestions), so the operator still sees the model's text.
///
/// `default_shell` is the operator's shell, used when the model omits one.
pub fn parse_copilot_answer(
    content: &str,
    default_shell: &str,
) -> (TerminalCopilotAnswer, ParseOutcome) {
    // Preferred shape: streamed prose, then a trailing ```json block.
    if let Some((prose, raw)) = parse_fenced_answer(content) {
        let explanation = if prose.trim().is_empty() {
            raw.explanation_md.trim()
        } else {
            prose.trim()
        };
        return (
            build_answer(explanation, raw.suggestions, default_shell),
            ParseOutcome::Structured,
        );
    }
    // Legacy fallback: a bare JSON object anywhere in the text.
    if let Some(raw) =
        extract_json_object(content).and_then(|j| serde_json::from_str::<RawAnswer>(j).ok())
    {
        return (
            build_answer(raw.explanation_md.trim(), raw.suggestions, default_shell),
            ParseOutcome::Structured,
        );
    }
    // Degrade: keep the raw text as the explanation so the operator still sees it.
    (
        TerminalCopilotAnswer {
            explanation_md: truncate_on_char_boundary(
                content.trim(),
                MAX_DEGRADED_EXPLANATION_BYTES,
            ),
            suggestions: Vec::new(),
        },
        ParseOutcome::Degraded,
    )
}

/// Locate the trailing ```json fenced block. Returns the prose before the fence
/// and the parsed raw answer inside it, or `None` if no parseable fenced block is
/// present. [`extract_json_object`] finds the balanced object that follows the
/// fence marker, tolerating the trailing ``` and a stream that dropped the closing
/// fence before the JSON object itself completed.
fn parse_fenced_answer(content: &str) -> Option<(&str, RawAnswer)> {
    let marker = content.rfind("```json")?;
    let prose = &content[..marker];
    let raw = extract_json_object(&content[marker..])
        .and_then(|j| serde_json::from_str::<RawAnswer>(j).ok())?;
    Some((prose, raw))
}

/// Assemble the answer, dropping empty-command suggestions and stamping the
/// server-authoritative risk / decision on each. The explanation is length-capped.
fn build_answer(
    explanation: &str,
    raws: Vec<RawSuggestion>,
    default_shell: &str,
) -> TerminalCopilotAnswer {
    let suggestions = raws
        .into_iter()
        .filter(|s| !s.command.trim().is_empty())
        .map(|s| finalize_suggestion(s, default_shell))
        .collect();
    TerminalCopilotAnswer {
        explanation_md: truncate_on_char_boundary(explanation, MAX_DEGRADED_EXPLANATION_BYTES),
        suggestions,
    }
}

/// Stamp the server-authoritative `risk` / `decision` onto one model suggestion.
fn finalize_suggestion(raw: RawSuggestion, default_shell: &str) -> CommandSuggestion {
    let shell = if raw.shell.trim().is_empty() {
        default_shell.to_string()
    } else {
        raw.shell
    };
    let input = ExecInput {
        target: ExecTarget::Shell {
            shell: shell.clone(),
        },
        command: raw.command.clone(),
        cwd: raw.cwd.clone(),
        timeout_ms: 0,
        max_stdout_bytes: 0,
        max_stderr_bytes: 0,
    };
    let classification = classify_command(&input).classification;
    CommandSuggestion {
        command: raw.command,
        shell,
        cwd: raw.cwd,
        note: raw.note,
        risk: classification.risk,
        decision: classification.decision,
    }
}

/// Emits assembled [`TerminalCopilotEvent`] frames over a runtime's outbound
/// channel. The web portable runtime serializes each into a signaling frame and
/// broadcasts it; the manager forwards each over its cross-instance stream. A
/// blanket impl covers any `Fn(TerminalCopilotEvent)`, so a runtime passes a
/// closure that forwards to its own sink.
pub trait CopilotFrameSink {
    fn emit(&self, event: TerminalCopilotEvent);
}

impl<F: Fn(TerminalCopilotEvent)> CopilotFrameSink for F {
    fn emit(&self, event: TerminalCopilotEvent) {
        self(event)
    }
}

/// Byte length of the leading prose that is safe to stream: everything before the
/// first ``` code fence. The copilot prompt asks for the explanation as prose
/// followed by a single trailing ```json block, so the first fence marks the end
/// of the human-readable answer. A trailing run of up to two backticks is held
/// back, so a fence opener split across deltas is never streamed as prose.
fn prose_prefix_len(s: &str) -> usize {
    match s.find("```") {
        Some(idx) => idx,
        None => {
            let trailing = s.bytes().rev().take_while(|&b| b == b'`').count().min(2);
            s.len() - trailing
        }
    }
}

/// A [`TurnSink`] that maps the agentic loop's lifecycle onto
/// [`TerminalCopilotEvent`] frames with a monotonic `seq`, forwarding each to a
/// [`CopilotFrameSink`]. Shared by both runtimes so they can never drift on frame
/// shape, sequencing, or terminal semantics.
///
/// Text streaming is opt-in via [`streaming_text`](Self::streaming_text). When
/// off (the default), only read-tool progress (`ToolStarted`) and the terminal
/// `Final` / `Error` frames are emitted — the agentic manager path keeps this, as
/// its intermediate tool-deciding turns must not leak half-text. When on (the
/// single-turn signal path), the explanation prose is forwarded as `Partial`
/// frames as it arrives; the trailing ```json suggestions block is withheld (the
/// `Final` frame carries the parsed, classified suggestions). A `terminated` latch
/// guarantees at most one terminal frame per request.
pub struct CopilotStreamSink<S> {
    sink: S,
    request_id: String,
    seq: u32,
    terminated: bool,
    uncommitted_partial: bool,
    /// Whether to forward explanation prose as `Partial` frames (opt-in).
    stream_text: bool,
    /// Assembled assistant text so far (only tracked when `stream_text`).
    text: String,
    /// Byte length of `text` already emitted as prose, so each delta streams only
    /// the new safe prefix.
    emitted: usize,
    /// Machine-readable AI marking stamped onto the terminal `Final` frame, when
    /// the upper layer (which knows the model and has a clock) injected one. This
    /// crate has neither, so it carries the pre-built stamp rather than building
    /// it here.
    provenance: Option<AiProvenance>,
}

impl<S: CopilotFrameSink> CopilotStreamSink<S> {
    /// Build a sink streaming one copilot request's frames to `sink`. Text
    /// streaming is off by default; call [`streaming_text`](Self::streaming_text)
    /// to enable explanation streaming.
    pub fn new(sink: S, request_id: impl Into<String>) -> Self {
        Self {
            sink,
            request_id: request_id.into(),
            seq: 0,
            terminated: false,
            uncommitted_partial: false,
            stream_text: false,
            text: String::new(),
            emitted: 0,
            provenance: None,
        }
    }

    /// Enable forwarding the explanation prose as `Partial` frames. Used by the
    /// single-turn signal path, where the whole turn's text is the answer.
    pub fn streaming_text(mut self) -> Self {
        self.stream_text = true;
        self
    }

    /// Inject the AI provenance to stamp onto the terminal `Final` frame. The
    /// upper layer builds it (it knows the model and has a clock, which this crate
    /// lacks) and sets it once the model is resolved, before the answer is
    /// emitted; this crate only carries it through.
    pub fn set_provenance(&mut self, provenance: AiProvenance) {
        self.provenance = Some(provenance);
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    /// Emit the terminal `Final` frame carrying the structured answer, unless a
    /// terminal frame was already sent.
    pub fn emit_final(&mut self, answer: TerminalCopilotAnswer) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        let mut frame = TerminalCopilotEvent::final_answer(self.request_id.clone(), seq, answer);
        if let Some(provenance) = &self.provenance {
            frame = frame.with_provenance(provenance.clone());
        }
        self.sink.emit(frame);
        self.uncommitted_partial = false;
        self.terminated = true;
    }

    /// Emit a terminal `Error` frame, unless a terminal frame was already sent.
    pub fn emit_error(&mut self, error: AgentError) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink.emit(TerminalCopilotEvent::error(
            self.request_id.clone(),
            seq,
            error,
        ));
        self.uncommitted_partial = false;
        self.terminated = true;
    }

    /// Whether a terminal frame (final or error) has been emitted.
    pub fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<S: CopilotFrameSink> TurnSink for CopilotStreamSink<S> {
    fn on_text_delta(&mut self, delta: &str) {
        // Off by default (the agentic manager path); only the single-turn signal
        // path opts in. The trailing ```json suggestions block is withheld — the
        // `Final` frame carries the parsed, classified answer.
        if !self.stream_text || self.terminated || delta.is_empty() {
            return;
        }
        self.text.push_str(delta);
        let safe = prose_prefix_len(&self.text);
        if safe > self.emitted {
            let fragment = self.text[self.emitted..safe].to_string();
            self.emitted = safe;
            let seq = self.next_seq();
            self.sink.emit(TerminalCopilotEvent::partial(
                self.request_id.clone(),
                seq,
                fragment,
            ));
            self.uncommitted_partial = true;
        }
    }

    fn on_partial_committed(&mut self) {
        if self.terminated {
            return;
        }
        if self.uncommitted_partial {
            let seq = self.next_seq();
            self.sink.emit(TerminalCopilotEvent::partial_committed(
                self.request_id.clone(),
                seq,
            ));
        }
        self.uncommitted_partial = false;
        self.text.clear();
        self.emitted = 0;
    }

    fn on_turn_retracted(&mut self, reason: StreamRetractionReason, error: Option<AgentError>) {
        if self.terminated {
            return;
        }
        if self.uncommitted_partial {
            let seq = self.next_seq();
            self.sink.emit(TerminalCopilotEvent::retracted(
                self.request_id.clone(),
                seq,
                reason,
                error,
            ));
            self.uncommitted_partial = false;
            self.text.clear();
            self.emitted = 0;
            self.terminated = true;
        } else if let Some(error) = error {
            self.emit_error(error);
        }
    }

    fn on_tool_started(&mut self, tool_name: &str, _call_id: &str, _arguments_json: &str) {
        if self.terminated {
            return;
        }
        let seq = self.next_seq();
        self.sink.emit(TerminalCopilotEvent::tool_started(
            self.request_id.clone(),
            seq,
            tool_name,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::ExecDecision;
    use desk_agent_protocol::terminal_copilot::{TerminalContext, TerminalCopilotEventKind};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn ask(mode: TerminalCopilotMode) -> TerminalCopilotAsk {
        TerminalCopilotAsk {
            conversation_id: None,
            mode,
            question: Some("free port 8080".into()),
            locale: None,
            history: Vec::new(),
            context: TerminalContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: "bind: address already in use".into(),
                last_command: Some("./server".into()),
                error_text: Some("address already in use".into()),
            },
            model_id: None,
            org_id: None,
        }
    }

    #[test]
    fn system_prompt_differs_by_mode_and_states_constraints() {
        let howto = build_copilot_system_message(TerminalCopilotMode::HowTo, None).text;
        let explain = build_copilot_system_message(TerminalCopilotMode::ExplainError, None).text;
        assert!(howto.contains("Propose the command"));
        assert!(explain.contains("root cause"));
        for p in [howto, explain] {
            assert!(p.contains("never execute"));
            // The answer is streamed prose followed by a trailing ```json block.
            assert!(p.contains("Markdown prose"));
            assert!(p.contains("```json"));
            assert_eq!(COPILOT_PROMPT_VERSION, "copilot-v1");
            assert!(p.contains("political figures"));
            assert!(p.contains("incidental technical"));
            assert!(p.contains("self-harm instructions"));
        }
    }

    #[test]
    fn system_prompt_injects_locale_language_rule_when_present() {
        let neutral = build_copilot_system_message(TerminalCopilotMode::HowTo, None).text;
        assert!(!neutral.contains("UI language"));
        // A blank / whitespace locale is treated as absent.
        let blank = build_copilot_system_message(TerminalCopilotMode::HowTo, Some("  ")).text;
        assert!(!blank.contains("UI language"));
        // A real locale steers the answer language and is echoed into the prompt.
        let localized =
            build_copilot_system_message(TerminalCopilotMode::ExplainError, Some("zh-CN")).text;
        assert!(localized.contains("UI language"));
        assert!(localized.contains("zh-CN"));
    }

    #[test]
    fn user_message_caps_recent_output() {
        let mut a = ask(TerminalCopilotMode::HowTo);
        a.context.recent_output = "x".repeat(MAX_RECENT_OUTPUT_BYTES * 2);
        let msg = build_copilot_user_message(&a);
        assert!(msg.text.contains("OS: linux"));
        assert!(msg.text.contains("Request: free port 8080"));
        // The forwarded recent output is capped (plus the small fixed preamble).
        assert!(msg.text.len() < MAX_RECENT_OUTPUT_BYTES + 512);
    }

    #[test]
    fn history_messages_alternate_roles_in_order() {
        let history = vec![
            CopilotHistoryTurn {
                user: "list listeners".into(),
                assistant: "use ss".into(),
            },
            CopilotHistoryTurn {
                user: "now kill it".into(),
                assistant: "use kill".into(),
            },
        ];
        let msgs = build_copilot_history_messages(&history);
        let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::User,
                ChatRole::Assistant,
            ]
        );
        assert_eq!(msgs[0].text, "list listeners");
        assert_eq!(msgs[3].text, "use kill");
    }

    #[test]
    fn history_keeps_only_the_last_n_turns() {
        let history: Vec<CopilotHistoryTurn> = (0..(MAX_COPILOT_HISTORY_TURNS + 3))
            .map(|i| CopilotHistoryTurn {
                user: format!("u{i}"),
                assistant: format!("a{i}"),
            })
            .collect();
        let msgs = build_copilot_history_messages(&history);
        // Two messages per kept turn.
        assert_eq!(msgs.len(), MAX_COPILOT_HISTORY_TURNS * 2);
        // The oldest kept turn is the (len - N)th, so the first three are dropped.
        assert_eq!(msgs[0].text, "u3");
    }

    #[test]
    fn history_caps_message_bytes_and_skips_empty() {
        let history = vec![
            CopilotHistoryTurn {
                user: "x".repeat(MAX_COPILOT_HISTORY_MSG_BYTES * 2),
                assistant: "   ".into(),
            },
            CopilotHistoryTurn {
                user: "  ".into(),
                assistant: "kept".into(),
            },
        ];
        let msgs = build_copilot_history_messages(&history);
        // The blank user / blank assistant are skipped; the long user is capped.
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].text.len() <= MAX_COPILOT_HISTORY_MSG_BYTES);
        assert_eq!(msgs[0].role, ChatRole::User);
        assert_eq!(msgs[1].text, "kept");
        assert_eq!(msgs[1].role, ChatRole::Assistant);
    }

    #[test]
    fn read_tool_subset_is_exactly_two() {
        let names: Vec<String> = copilot_read_tools()
            .into_iter()
            .map(|t| t.spec.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"read_system_info".to_string()));
        assert!(names.contains(&"read_process_list".to_string()));
    }

    #[test]
    fn structured_answer_classifies_each_suggestion() {
        let content = r#"Here you go:
        {"explanation_md": "List the listener.",
         "suggestions": [{"command": "ss -ltnp sport = :8080", "shell": "bash", "cwd": null, "note": "list listener"}]}"#;
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(answer.explanation_md, "List the listener.");
        assert_eq!(answer.suggestions.len(), 1);
        // An off-template read command is suggest-only (not AI-executable).
        assert_eq!(answer.suggestions[0].decision, ExecDecision::NotExecutable);
    }

    #[test]
    fn blocked_command_is_classified_blocked_not_self_reported() {
        // The model tries to self-report a benign decision; the server ignores it
        // and the blocklist classifier wins.
        let content = r#"{"explanation_md": "x",
            "suggestions": [{"command": "cat /etc/shadow", "shell": "bash", "note": "read", "decision": "confirm_required", "risk": "low"}]}"#;
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(answer.suggestions[0].decision, ExecDecision::Blocked);
        assert_eq!(answer.suggestions[0].risk, RiskLevel::Blocked);
    }

    #[test]
    fn empty_command_suggestions_are_dropped() {
        let content = r#"{"explanation_md": "x", "suggestions": [{"command": "   ", "shell": "bash", "note": "n"}]}"#;
        let (answer, _) = parse_copilot_answer(content, "bash");
        assert!(answer.suggestions.is_empty());
    }

    #[test]
    fn malformed_json_degrades_to_explanation_only() {
        let (answer, outcome) = parse_copilot_answer("sorry, no JSON here", "bash");
        assert_eq!(outcome, ParseOutcome::Degraded);
        assert_eq!(answer.explanation_md, "sorry, no JSON here");
        assert!(answer.suggestions.is_empty());
    }

    /// The preferred shape: streamed prose, then a trailing ```json block. The
    /// prose is the explanation; the fenced JSON yields the (classified)
    /// suggestions, even though it omits `explanation_md`.
    #[test]
    fn fenced_answer_takes_prose_as_explanation() {
        let content = "Port 8080 is held by a stale listener.\n\nKill it:\n```json\n\
            {\"suggestions\": [{\"command\": \"ss -ltnp sport = :8080\", \"shell\": \"bash\", \"note\": \"list listener\"}]}\n```";
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert!(answer.explanation_md.starts_with("Port 8080 is held"));
        // The fenced JSON block is not part of the explanation prose.
        assert!(!answer.explanation_md.contains("```"));
        assert!(!answer.explanation_md.contains("suggestions"));
        assert_eq!(answer.suggestions.len(), 1);
    }

    /// A fenced block whose JSON also carries `explanation_md` but with no prose
    /// before the fence falls back to the JSON's explanation.
    #[test]
    fn fenced_answer_without_prose_uses_json_explanation() {
        let content = "```json\n{\"explanation_md\": \"from json\", \"suggestions\": []}\n```";
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(answer.explanation_md, "from json");
        assert!(answer.suggestions.is_empty());
    }

    /// A stream truncated before the closing fence still parses, as long as the
    /// JSON object itself is complete.
    #[test]
    fn fenced_answer_tolerates_missing_closing_fence() {
        let content = "Here is the fix.\n```json\n\
            {\"suggestions\": [{\"command\": \"ls\", \"shell\": \"bash\", \"note\": \"list\"}]}";
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(answer.explanation_md, "Here is the fix.");
        assert_eq!(answer.suggestions.len(), 1);
    }

    /// Prose containing `{placeholder}` braces does not confuse the trailing-fence
    /// extraction (the authoritative JSON is the fenced block, not the prose).
    #[test]
    fn fenced_answer_ignores_braces_in_prose() {
        let content = "Set {VAR} then run it.\n```json\n{\"suggestions\": []}\n```";
        let (answer, outcome) = parse_copilot_answer(content, "bash");
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(answer.explanation_md, "Set {VAR} then run it.");
        assert!(answer.suggestions.is_empty());
    }

    /// An empty ```json block has no object to parse and no bare object elsewhere,
    /// so it degrades (the operator still sees the text).
    #[test]
    fn empty_fenced_block_degrades() {
        let (answer, outcome) = parse_copilot_answer("no suggestions\n```json\n```", "bash");
        assert_eq!(outcome, ParseOutcome::Degraded);
        assert!(answer.suggestions.is_empty());
        assert!(answer.explanation_md.contains("no suggestions"));
    }

    /// `prose_prefix_len` returns everything before the first fence, and holds back
    /// a trailing partial-fence backtick run when no fence is present yet.
    #[test]
    fn prose_prefix_stops_at_fence_and_holds_partial_backticks() {
        assert_eq!(prose_prefix_len("hello world"), "hello world".len());
        assert_eq!(prose_prefix_len("hello ```json"), "hello ".len());
        // A trailing run of up to two backticks (a fence opener mid-arrival) is
        // withheld until the next delta resolves it.
        assert_eq!(prose_prefix_len("hello ``"), "hello ".len());
        assert_eq!(prose_prefix_len("hello `"), "hello ".len());
    }

    /// With text streaming enabled, prose is forwarded as `Partial` frames and the
    /// trailing ```json block never leaks; the `Final` frame carries the answer.
    #[test]
    fn streaming_sink_forwards_prose_and_withholds_json_block() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "req-s").streaming_text();
        s.on_text_delta("Free the ");
        s.on_text_delta("port.\n");
        s.on_text_delta("```json\n");
        s.on_text_delta("{\"suggestions\": []}\n```");
        s.emit_final(TerminalCopilotAnswer {
            explanation_md: "Free the port.".into(),
            suggestions: Vec::new(),
        });

        let ev = store.borrow();
        let partials: String = ev
            .iter()
            .filter(|e| e.kind == TerminalCopilotEventKind::Partial)
            .filter_map(|e| e.partial_text.clone())
            .collect();
        assert_eq!(partials, "Free the port.\n");
        assert!(!partials.contains("```"));
        assert!(!partials.contains("suggestions"));
        // Monotonic seq across partials then the final frame.
        let seqs: Vec<u32> = ev.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted);
        assert_eq!(ev.last().unwrap().kind, TerminalCopilotEventKind::Final);
    }

    /// Text streaming is off by default, so the agentic manager path emits no
    /// `Partial` frames even as deltas arrive.
    #[test]
    fn default_sink_drops_text_deltas() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "req-d");
        s.on_text_delta("some streamed prose");
        assert!(store.borrow().is_empty());
    }

    /// A recording frame sink: a closure pushing each frame into a shared buffer.
    fn recorder() -> (
        Rc<RefCell<Vec<TerminalCopilotEvent>>>,
        impl Fn(TerminalCopilotEvent),
    ) {
        let store = Rc::new(RefCell::new(Vec::new()));
        let s = store.clone();
        (store, move |e| s.borrow_mut().push(e))
    }

    /// A read-tool turn maps to ToolStarted → Final with a gapless monotonic seq,
    /// the right kind on each frame, and the terminated latch set after Final.
    #[test]
    fn stream_sink_maps_tool_then_final_in_order() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "req-1");
        s.on_tool_started("read_system_info", "c1", "{}");
        s.emit_final(TerminalCopilotAnswer {
            explanation_md: "done".into(),
            suggestions: Vec::new(),
        });

        let ev = store.borrow();
        let kinds: Vec<_> = ev.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TerminalCopilotEventKind::ToolStarted,
                TerminalCopilotEventKind::Final,
            ]
        );
        assert_eq!(ev.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1]);
        assert!(ev.iter().all(|e| e.request_id == "req-1"));
        assert_eq!(ev[0].tool_name.as_deref(), Some("read_system_info"));
        assert!(ev[1].is_terminal());
        assert!(s.is_terminated());
    }

    /// An injected provenance is stamped onto the terminal `Final` frame; a sink
    /// without one still emits the answer (fail-closed at the UI, not here).
    #[test]
    fn stream_sink_stamps_injected_provenance_on_final() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "req-p");
        s.set_provenance(AiProvenance::stamp(
            Some("gpt-4o".into()),
            Some("2026-07-14T00:00:00Z".into()),
        ));
        s.emit_final(TerminalCopilotAnswer {
            explanation_md: "done".into(),
            suggestions: Vec::new(),
        });
        let ev = store.borrow();
        let prov = ev[0]
            .provenance
            .as_ref()
            .expect("final frame carries the injected provenance");
        assert_eq!(prov.model_id.as_deref(), Some("gpt-4o"));
    }

    /// With no provenance injected, the Final frame simply omits it.
    #[test]
    fn stream_sink_final_without_provenance_omits_it() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "req-n");
        s.emit_final(TerminalCopilotAnswer {
            explanation_md: "done".into(),
            suggestions: Vec::new(),
        });
        assert!(store.borrow()[0].provenance.is_none());
    }

    /// Once a terminal frame is emitted, every later frame is suppressed — exactly
    /// one terminal per request.
    #[test]
    fn stream_sink_latches_after_terminal() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "r");
        s.emit_final(TerminalCopilotAnswer {
            explanation_md: "a".into(),
            suggestions: Vec::new(),
        });
        // All of these are dropped by the latch.
        s.emit_error(AgentError {
            kind: desk_agent_protocol::AgentErrorKind::Internal,
            message: "late".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
        s.on_tool_started("t", "c", "{}");
        let ev = store.borrow();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, TerminalCopilotEventKind::Final);
    }

    /// A turn that fails before answering emits a single terminal `Error`.
    #[test]
    fn stream_sink_error_is_terminal() {
        let (store, sink) = recorder();
        let mut s = CopilotStreamSink::new(sink, "r");
        s.emit_error(AgentError {
            kind: desk_agent_protocol::AgentErrorKind::SessionUnavailable,
            message: "busy".into(),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        });
        let ev = store.borrow();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, TerminalCopilotEventKind::Error);
        assert!(ev[0].is_terminal());
        assert!(s.is_terminated());
    }
    #[test]
    fn stream_sink_retracts_provisional_text_once_and_ignores_late_delta() {
        use desk_agent_protocol::content_safety::StreamRetractionReason;

        let (store, sink) = recorder();
        let mut stream = CopilotStreamSink::new(sink, "r").streaming_text();
        stream.on_text_delta("unsafe provisional text");
        stream.on_turn_retracted(
            StreamRetractionReason::PolicyBlocked,
            Some(crate::content_safety::content_blocked_error()),
        );
        stream.on_text_delta("late");
        stream.emit_error(crate::content_safety::content_safety_unavailable());

        let events = store.borrow();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, TerminalCopilotEventKind::Partial);
        assert_eq!(events[1].kind, TerminalCopilotEventKind::Retracted);
        assert!(events[1].is_terminal());
        assert_eq!(
            events[1].retraction_reason,
            Some(StreamRetractionReason::PolicyBlocked)
        );
    }

    #[test]
    fn stream_sink_policy_failure_without_partial_is_error() {
        use desk_agent_protocol::content_safety::StreamRetractionReason;

        let (store, sink) = recorder();
        let mut stream = CopilotStreamSink::new(sink, "r").streaming_text();
        stream.on_turn_retracted(
            StreamRetractionReason::SafetyUnavailable,
            Some(crate::content_safety::content_safety_unavailable()),
        );

        let events = store.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TerminalCopilotEventKind::Error);
        assert!(events[0].is_terminal());
    }

    #[test]
    fn stream_sink_commits_before_tool_and_uses_error_after_commit() {
        use desk_agent_protocol::content_safety::StreamRetractionReason;

        let (store, sink) = recorder();
        let mut stream = CopilotStreamSink::new(sink, "r").streaming_text();
        stream.on_text_delta("reviewed reasoning");
        stream.on_partial_committed();
        stream.on_tool_started("read_system_info", "c1", "{}");
        stream.on_turn_retracted(
            StreamRetractionReason::PolicyBlocked,
            Some(crate::content_safety::content_blocked_error()),
        );

        let events = store.borrow();
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                TerminalCopilotEventKind::Partial,
                TerminalCopilotEventKind::PartialCommitted,
                TerminalCopilotEventKind::ToolStarted,
                TerminalCopilotEventKind::Error,
            ]
        );
        assert!(events.last().unwrap().is_terminal());
    }
}
