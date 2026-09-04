import { describe, expect, it } from 'vitest';

import { isExactExternalSendTool, parseExternalSendReceipt } from './device-assistant-external-send';

const digest = 'a'.repeat(64);
const base = {
    schema_version: 4,
    snapshot_id: 'snapshot-1',
    snapshot_sha256: digest,
    idempotency_key: `send:v1:${digest}`,
    observed_at_unix_ms: 42,
};

describe('external send receipts', () => {
    it('accepts only outcome-specific evidence and receipt identity', () => {
        expect(parseExternalSendReceipt(JSON.stringify({
            ...base,
            outcome: 'sent',
            provider_receipt_id: 'provider-receipt-1',
            evidence: 'provider_ui_acknowledgement',
        }))?.outcome).toBe('sent');
        expect(parseExternalSendReceipt(JSON.stringify({
            ...base,
            outcome: 'definitely_not_sent',
            provider_receipt_id: null,
            evidence: 'precondition_rejected_before_activation',
        }))?.outcome).toBe('definitely_not_sent');
        expect(parseExternalSendReceipt(JSON.stringify({
            ...base,
            outcome: 'outcome_unknown',
            provider_receipt_id: null,
            evidence: 'receipt_not_observed_after_activation',
        }))?.outcome).toBe('outcome_unknown');

        expect(parseExternalSendReceipt(JSON.stringify({
            ...base,
            outcome: 'sent',
            provider_receipt_id: null,
            evidence: 'receipt_not_observed_after_activation',
        }))).toBeNull();
        expect(parseExternalSendReceipt(JSON.stringify({
            ...base,
            snapshot_sha256: 'bad',
            outcome: 'sent',
            provider_receipt_id: 'receipt',
            evidence: 'provider_ui_acknowledgement',
        }))).toBeNull();
    });

    it('recognizes only the two reviewed exact-send tools', () => {
        expect(isExactExternalSendTool('send_gmail_web_exact')).toBe(true);
        expect(isExactExternalSendTool('send_slack_web_exact')).toBe(true);
        expect(isExactExternalSendTool('prepare_gmail_web_draft_handoff')).toBe(false);
        expect(isExactExternalSendTool('browser_activate_element')).toBe(false);
    });
});
