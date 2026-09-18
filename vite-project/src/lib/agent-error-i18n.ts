import type { TFunction } from 'i18next';
import { deskErrorCodeEnum } from '@/services/types';
import { deskErrorMessage, type ErrorCodeKeyMap } from '@/lib/desk-error-i18n';

/**
 * Localize an agent error by its machine-readable `error_code`.
 *
 * Agent errors (assistant / diagnose / terminal-complete / exec) ride the
 * `AgentError` wire shape, which carries an optional `error_code`. The backend
 * sends only the numeric code plus a raw English `message`; the control end maps
 * the code to a localized string here so the UI is never English-only. An error
 * without a known code falls back to the backend `message`.
 */

/** Codes reaching the agent-error wire that have a dedicated localized message. */
const CODE_TO_KEY: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.SCHEDULE_MODEL_BUDGET_EXCEEDED]:
        'pages.agentError.scheduleModelBudgetExceeded',
    [deskErrorCodeEnum.TERMINAL_AI_ASSISTANT_DISABLED]: 'pages.agentError.terminalAiAssistantDisabled',
    [deskErrorCodeEnum.AI_MODEL_NOT_CONFIGURED]: 'pages.agentError.aiModelNotConfigured',
    [deskErrorCodeEnum.AI_ASSISTANT_STEP_LIMIT_EXCEEDED]: 'pages.agentError.assistantStepLimit',
    [deskErrorCodeEnum.AI_ASSISTANT_RESPONSE_TRUNCATED]: 'pages.agentError.assistantTruncated',
    [deskErrorCodeEnum.AI_ASSISTANT_PROTOCOL_VIOLATION]: 'pages.agentError.assistantProtocolViolation',
    [deskErrorCodeEnum.AI_ASSISTANT_TURN_BUSY]: 'pages.agentError.assistantTurnBusy',
    [deskErrorCodeEnum.AI_ASSISTANT_SUBJECT_MISMATCH]: 'pages.agentError.assistantSubjectMismatch',
    [deskErrorCodeEnum.AGENT_SAME_TOOL_REPEAT_LIMIT]: 'pages.agentError.sameToolRepeatLimit',
    [deskErrorCodeEnum.RATE_LIMITED]: 'pages.agentError.aiRateDisabled',
    [deskErrorCodeEnum.AI_CONTEXT_LIMIT_EXCEEDED]: 'pages.agentError.aiContextLimitExceeded',
    [deskErrorCodeEnum.AI_CONTEXT_ITEM_TOO_LARGE]: 'pages.agentError.aiContextItemTooLarge',
    [deskErrorCodeEnum.AI_CONTEXT_COMPRESSION_FAILED]:
        'pages.agentError.aiContextCompressionFailed',
    [deskErrorCodeEnum.AI_PLATFORM_BUSY]: 'pages.agentError.aiPlatformBusy',
    [deskErrorCodeEnum.AI_MODEL_IMAGE_INPUT_UNSUPPORTED]:
        'pages.agentError.aiModelImageInputUnsupported',
    [deskErrorCodeEnum.AI_CONTENT_BLOCKED]: 'pages.agentError.aiContentBlocked',
    [deskErrorCodeEnum.AI_CONTENT_SAFETY_UNAVAILABLE]:
        'pages.agentError.aiContentSafetyUnavailable',
    [deskErrorCodeEnum.AI_CONTENT_SAFETY_IMAGE_UNSUPPORTED]:
        'pages.agentError.aiContentSafetyImageUnsupported',
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
    return deskErrorMessage(t, CODE_TO_KEY, code, message, fallback);
}

export type ContentRetractionReason =
    | 'policy_blocked'
    | 'safe_redirect'
    | 'safety_unavailable'
    | 'incomplete';

const RETRACTION_KEY: Record<ContentRetractionReason, string> = {
    policy_blocked: 'pages.agentError.aiContentBlocked',
    safe_redirect: 'pages.agentError.aiContentSafeRedirect',
    safety_unavailable: 'pages.agentError.aiContentSafetyUnavailable',
    incomplete: 'pages.agentError.aiContentIncomplete',
};

/** Select fixed local text from a closed retraction reason; never show provider prose. */
export function contentRetractionMessage(
    t: TFunction,
    reason: ContentRetractionReason,
): string {
    return t(RETRACTION_KEY[reason]);
}
