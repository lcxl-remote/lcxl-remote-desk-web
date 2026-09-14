import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { parseFileReceipt, AssistantFileResult } from './assistant-file-result';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const mutation = (verified = true, operation = 'delete') => ({ result: verified ? 'verified' : 'outcome_unknown', output: { kind: 'text_file_mutation', value: { operation, original_file_name: 'notes.txt', original_size_bytes: 3, original_sha256: 'a'.repeat(64), recovery: { recovery_id: 'b'.repeat(64), created_at_unix_ms: 1, expires_at_unix_ms: 1000, cleanup_pending: false }, verified, updated_file: null } } });
describe('native file receipts', () => {
    it('exports the backup reference and leaves a readable failure that can be retried', async () => {
        const text = JSON.stringify(mutation());
        const download = vi.fn().mockRejectedValueOnce(new Error('Offline')).mockResolvedValue(undefined);
        render(<AssistantFileResult receipt={parseFileReceipt(text)!} text={text} onExportBackup={download} />);
        const button = screen.getByRole('button', { name: 'pages.fileRecovery.export', hidden: true });
        fireEvent.click(button);
        await waitFor(() => expect(screen.getByRole('alert', { hidden: true })).toBeTruthy());
        expect(download).toHaveBeenCalledWith('b'.repeat(64));
        fireEvent.click(button);
        await waitFor(() => expect(download).toHaveBeenCalledTimes(2));
        expect(screen.queryByRole('alert', { hidden: true })).toBeNull();
    });
    it('keeps recovery details collapsed and reports unknown without inventing success', () => {
        const text = JSON.stringify(mutation(false, 'update'));
        const receipt = parseFileReceipt(text)!;
        expect(receipt.operation).toBe('update');
        expect(receipt.verified).toBe(false);
        const { container } = render(<AssistantFileResult receipt={receipt} text={text} />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(screen.getByText('pages.deviceAssistant.fileReceipt.unknownHint')).toBeTruthy();
        expect(container.querySelector('a')).toBeNull();
    });
    it('keeps empty creation byte counts and rejects malformed or contradictory facts', () => {
        expect(parseFileReceipt(JSON.stringify({ result: 'verified', output: { kind: 'file_artifact', value: { file_name: 'empty.txt', size_bytes: 0, digest_sha256: 'a'.repeat(64) } } }))?.bytes).toBe(0);
        expect(parseFileReceipt(JSON.stringify(mutation()))?.operation).toBe('delete');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.fileName).toBe('notes.txt');
        expect(parseFileReceipt(JSON.stringify(mutation()))?.bytes).toBe(3);
        expect(parseFileReceipt(JSON.stringify({ ...mutation(), result: 'outcome_unknown' }))).toBeNull();
        const withoutReference = parseFileReceipt(JSON.stringify(mutation(true, 'update')));
        expect(withoutReference?.verified).toBe(true);
        expect(withoutReference?.referenceUnavailable).toBe(true);
        expect(withoutReference?.digest).toBeUndefined();
        expect(withoutReference?.bytes).toBeUndefined();
        expect(parseFileReceipt('{bad')).toBeNull();
    });
});
