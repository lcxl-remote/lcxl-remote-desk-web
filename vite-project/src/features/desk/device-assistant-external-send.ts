export type ExternalSendReceipt = {
    schema_version: number;
    snapshot_id: string;
    snapshot_sha256: string;
    idempotency_key: string;
    outcome: 'sent' | 'definitely_not_sent' | 'outcome_unknown';
    provider_receipt_id?: string | null;
    evidence:
        | 'provider_ui_acknowledgement'
        | 'precondition_rejected_before_activation'
        | 'receipt_not_observed_after_activation';
    observed_at_unix_ms: number;
};

const EXACT_SEND_TOOLS = new Set(['send_gmail_web_exact', 'send_slack_web_exact']);
const SHA256 = /^[0-9a-f]{64}$/;

export function isExactExternalSendTool(toolName: string) {
    return EXACT_SEND_TOOLS.has(toolName);
}

export function parseExternalSendReceipt(output: string | null): ExternalSendReceipt | null {
    if (!output) return null;
    try {
        const value = JSON.parse(output) as Partial<ExternalSendReceipt>;
        if (
            value.schema_version !== 4
            || typeof value.snapshot_id !== 'string'
            || value.snapshot_id.length === 0
            || typeof value.snapshot_sha256 !== 'string'
            || !SHA256.test(value.snapshot_sha256)
            || value.idempotency_key !== `send:v1:${value.snapshot_sha256}`
            || !Number.isSafeInteger(value.observed_at_unix_ms)
            || (value.observed_at_unix_ms ?? 0) <= 0
        ) return null;

        const hasReceipt = typeof value.provider_receipt_id === 'string'
            && value.provider_receipt_id.length > 0;
        const noReceipt = value.provider_receipt_id == null;
        const validOutcome =
            (value.outcome === 'sent'
                && value.evidence === 'provider_ui_acknowledgement'
                && hasReceipt)
            || (value.outcome === 'definitely_not_sent'
                && value.evidence === 'precondition_rejected_before_activation'
                && noReceipt)
            || (value.outcome === 'outcome_unknown'
                && value.evidence === 'receipt_not_observed_after_activation'
                && noReceipt);
        return validOutcome ? value as ExternalSendReceipt : null;
    } catch {
        return null;
    }
}
