
export const SIGNALING_TYPE_CODE_HEARTBEAT = 1;

export const SIGNALING_TYPE_CODE_FETCH_CONNECTIONS = 21;
export const SIGNALING_TYPE_CODE_CONNECTION_LIST = 22;

export const SIGNALING_TYPE_CODE_REQUEST_REMOTE = 100;
export const SIGNALING_TYPE_CODE_INIT = 101;
export const SIGNALING_TYPE_CODE_OFFER = 102;
export const SIGNALING_TYPE_CODE_ANSWER = 103;
export const SIGNALING_TYPE_CODE_CANID = 104;
export const SIGNALING_TYPE_CODE_MANAGER_FILE_LIST = 10005;
export const SIGNALING_TYPE_CODE_MANAGER_FILE_DELETE = 10006;

export const SIGNALING_TYPE_CODE_REQUIRE_CONTROL = 201;
export const SIGNALING_TYPE_CODE_ACCEPT_CONTROL = 202;
export const SIGNALING_TYPE_CODE_DENY_CONTROL = 203;
export const SIGNALING_TYPE_CODE_CLOSE_CONTROL = 204;
export const SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS = 205;

export const SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS = 301;

export const SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN = 206;
export const SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED = 207;
export const SIGNALING_TYPE_CODE_AUDIO_PLAYBACK_ERROR = 208;

export const SIGNALING_TYPE_CODE_DESKTOP_SWITCHING = 500;
export const SIGNALING_TYPE_CODE_DESKTOP_READY = 501;

// AI Diagnose: request (control end -> host) and streamed event frames
// (host -> control end). The event stream is notification-style — frames
// carry `request_id` + `seq` + `kind`, never a one-shot response.
export const SIGNALING_TYPE_CODE_DIAGNOSE = 602;
export const SIGNALING_TYPE_CODE_DIAGNOSE_EVENT = 603;
// Handoff to a human ("转人工"): control end -> host. The host records an
// `ai.task.cancelled` audit. Carries no payload; the message request_id
// correlates the cancelled diagnosis.
export const SIGNALING_TYPE_CODE_DIAGNOSE_CANCEL = 604;

// AI confirmed execution: ConfirmExec / ResolveExec (control end -> host) and
// ExecPreview / ExecResult (host -> control end, notification-style). The host
// classifies the command, requires explicit approval, and runs only whitelist
// templates.
export const SIGNALING_TYPE_CODE_CONFIRM_EXEC = 605;
export const SIGNALING_TYPE_CODE_EXEC_PREVIEW = 606;
export const SIGNALING_TYPE_CODE_RESOLVE_EXEC = 607;
export const SIGNALING_TYPE_CODE_EXEC_RESULT = 609;

// Execution lifecycle: ExecControl (control end → host: cancel or state query),
// and the host's answers — ExecStateReply (to both) and ExecLifecycle (accepted /
// still running). Mirror `SignalingType::Exec{Control,StateReply,Lifecycle}`.
// These let the control end show what a command is actually doing instead of
// assuming it started, and stop one that is running.
export const SIGNALING_TYPE_CODE_EXEC_CONTROL = 623;
export const SIGNALING_TYPE_CODE_EXEC_STATE_REPLY = 624;
export const SIGNALING_TYPE_CODE_EXEC_LIFECYCLE = 625;

// Terminal AI copilot: the ask (control end → server), the notification-style
// event stream (server → control end), and a cancel. Mirror
// `SignalingType::TerminalCopilot{Ask,Event,Cancel}` (617/618/619).
export const SIGNALING_TYPE_CODE_TERMINAL_COPILOT_ASK = 617;
export const SIGNALING_TYPE_CODE_TERMINAL_COPILOT_EVENT = 618;
export const SIGNALING_TYPE_CODE_TERMINAL_COPILOT_CANCEL = 619;

// AI command completion: a non-streaming ask + single result. Mirror
// `SignalingType::TerminalComplete{Ask,Result}` (620/621).
export const SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_ASK = 620;
export const SIGNALING_TYPE_CODE_TERMINAL_COMPLETE_RESULT = 621;

export const SIGNALING_TYPE_CODE_ERROR = 10000000;
export const SIGNALING_TYPE_CODE_UNKNOWN_TYPE = 10000001;
