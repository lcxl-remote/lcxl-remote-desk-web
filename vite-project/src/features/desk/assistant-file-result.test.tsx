import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { parseFileReceipt, AssistantFileResult } from './assistant-file-result';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const mutation = (verified = true, operation = 'delete') => ({ result: verified ? 'verified' : 'outcome_unknown', output: { kind: 'text_file_mutation', value: { operation, original_file_name: 'notes.txt', original_size_bytes: 3, original_sha256: 'a'.repeat(64), recovery_path: '/private/tmp/selected/.assistant-recovery-example', verified, updated_file: null } } });
describe('native file receipts', () => {
    it('keeps recovery details collapsed and reports unknown without inventing success', () => {
        const text = JSON.stringify(mutation(false, 'update'));
        const receipt = parseFileReceipt(text)!;
        expect(receipt.operation).toBe('update');
        expect(receipt.verified).toBe(false);
        const { container } = render(<AssistantFileResult receipt={receipt} text={text} />);
        expect(container.querySelector('details')?.open).toBe(false);
        expect(screen.getByText('pages.deviceAssistant.fileReceipt.unknownHint')).toBeTruthy();
        expect(container.querySelector('a')).toBeNull();
    });
    it('keeps empty creation byte counts and rejects malformed or contradictory facts', () => {
        expect(parseFileReceipt(JSON.stringify({ result: 'verified', output: { kind: 'file_artifact', value: { file_name: 'empty.txt', size_bytes: 0, digest_sha256: 'a'.repeat(64) } } }))?.bytes).toBe(0);
        expect(parseFileReceipt(JSON.stringify(mutation()))?.operation).toBe('delete');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.fileName).toBe('notes.txt');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.bytes).toBe(3);
        expect(parseFileReceipt(JSON.stringify({ ...mutation(), result: 'outcome_unknown' }))).toBeNull();
        expect(parseFileReceipt(JSON.stringify(mutation(true, 'update')))).toBeNull();
        expect(parseFileReceipt('{bad')).toBeNull();
    });
});
