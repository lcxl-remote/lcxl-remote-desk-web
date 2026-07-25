import type { TFunction } from 'i18next';

/**
 * Localize an agent error by its machine-readable `error_code`.
 *
 * Agent errors (copilot / diagnose / terminal-complete / exec) ride the
 * `AgentError` wire shape, which carries an optional `error_code` (a
 * `DeskErrorCode` value from `web/utils/src/error.rs`). The backend sends only
 * the numeric code plus a raw English `message`; the control end maps the code
 * to a localized string here so the UI is never English-only. An error without a
 * known code falls back to the backend `message`.
 */

/** Mirror of the `DeskErrorCode` values that ride the agent-error wire. */
export const AGENT_ERROR_CODE = {
    TERMINAL_COPILOT_DISABLED: 50,
    AI_MODEL_NOT_CONFIGURED: 51,
    COPILOT_STEP_LIMIT_EXCEEDED: 57,
    COPILOT_RESPONSE_TRUNCATED: 58,
    COPILOT_PROTOCOL_VIOLATION: 59,
    COPILOT_TURN_BUSY: 60,
    COPILOT_SUBJECT_MISMATCH: 61,
    AGENT_SAME_TOOL_REPEAT_LIMIT: 70,
} as const;

/** Codes with a dedicated localized message. */
const CODE_TO_KEY: Record<number, string> = {
    [AGENT_ERROR_CODE.TERMINAL_COPILOT_DISABLED]: 'pages.agentError.terminalCopilotDisabled',
    [AGENT_ERROR_CODE.AI_MODEL_NOT_CONFIGURED]: 'pages.agentError.aiModelNotConfigured',
    [AGENT_ERROR_CODE.COPILOT_STEP_LIMIT_EXCEEDED]: 'pages.agentError.copilotStepLimit',
    [AGENT_ERROR_CODE.COPILOT_RESPONSE_TRUNCATED]: 'pages.agentError.copilotTruncated',
    [AGENT_ERROR_CODE.COPILOT_PROTOCOL_VIOLATION]: 'pages.agentError.copilotProtocolViolation',
    [AGENT_ERROR_CODE.COPILOT_TURN_BUSY]: 'pages.agentError.copilotTurnBusy',
    [AGENT_ERROR_CODE.COPILOT_SUBJECT_MISMATCH]: 'pages.agentError.copilotSubjectMismatch',
    [AGENT_ERROR_CODE.AGENT_SAME_TOOL_REPEAT_LIMIT]: 'pages.agentError.sameToolRepeatLimit',
};

/**
 * Resolve a display message for an agent error. Prefers the localized message
 * for a known `code`; otherwise returns `message` (the backend text) or, if that
 * is empty, `fallback`.
 */
export function agentErrorMessage(
    t: TFunction,
    code: number | null | undefined,
    message: string | null | undefined,
    fallback: string,
): string {
    if (code != null && CODE_TO_KEY[code]) {
        return t(CODE_TO_KEY[code]);
    }
    return message && message.length > 0 ? message : fallback;
}
