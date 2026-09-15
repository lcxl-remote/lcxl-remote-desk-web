import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { parseFileReceipt, AssistantFileResult } from './assistant-file-result';
import { AssistantCommandResult } from './assistant-command-result';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const mutation = (verified = true, operation = 'delete') => ({ result: verified ? 'verified' : 'outcome_unknown', output: { kind: 'text_file_mutation', value: { operation, original_file_name: 'notes.txt', original_size_bytes: 3, original_sha256: 'a'.repeat(64), recovery: { recovery_id: 'b'.repeat(64), created_at_unix_ms: 1, expires_at_unix_ms: 1000, cleanup_pending: false }, verified, updated_file: null } } });
describe('native file receipts', () => {
    it('presents Windows and macOS batch copies through the same file receipt', () => {
        for (const fileName of ['copy.pptx', 'copy.docx', `${'a'.repeat(250)}.DOCX`, 'copy.xlsx', `${'a'.repeat(250)}.XLSX`, 'copy.key', 'copy.pages', 'copy.numbers']) {
            const fileOnly = /\.(pptx|docx|xlsx)$/i.test(fileName);
            const value = fileOnly
                ? { file_name: fileName, size_bytes: 123, digest_sha256: 'a'.repeat(64) }
                : { file_name: fileName, byte_len: 123, sha256: 'a'.repeat(64), validation_byte_len: 32, validation_sha256: 'b'.repeat(64) };
            const output = { kind: fileOnly ? 'file_artifact' : 'batch_document_artifact', value };
            expect(parseFileReceipt(JSON.stringify({ result: 'verified', output }))).toEqual({
                operation: 'create', verified: true, fileName, bytes: 123, digest: 'a'.repeat(64),
            });
            for (const result of ['outcome_unknown', 'definitely_not_started', 'changed_but_unverified']) {
                expect(parseFileReceipt(JSON.stringify({ result, output }))).toBeNull();
            }
        }
        const value = { file_name: 'copy.key', byte_len: 123, sha256: 'a'.repeat(64), validation_byte_len: 32, validation_sha256: 'b'.repeat(64) };
        for (const changed of [{ byte_len: -1 }, { validation_byte_len: 0 }, { validation_sha256: 'bad' }, { file_name: '' }]) {
            expect(parseFileReceipt(JSON.stringify({ result: 'verified', output: { kind: 'batch_document_artifact', value: { ...value, ...changed } } }))).toBeNull();
        }
    });
    it.each([
        ['Word', 'DOCX', 'application/vnd.openxmlformats-officedocument.wordprocessingml.document'],
        ['Excel', 'XLSX', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet'],
    ])('renders the verified %s copy through the shared result component without granting another action', (_host, extension, mediaType) => {
        const fileName = `${'报告 '.repeat(35)}x.${extension}`;
        const output = { kind: 'file_artifact', value: {
            file_name: fileName, size_bytes: 123, digest_sha256: 'a'.repeat(64),
            media_type: mediaType,
        } };
        const { container, rerender } = render(<AssistantCommandResult text={JSON.stringify({ result: 'verified', output })} />);
        const title = screen.getByRole('button', { name: /fileReceipt.title.*fileReceipt.create.*fileReceipt.verified/ });
        expect(title.getAttribute('aria-expanded')).toBe('false');
        fireEvent.click(title);
        expect(title.getAttribute('aria-expanded')).toBe('true');
        expect(screen.getByText(fileName)).toBeVisible();
        expect(screen.getByText('123')).toBeVisible();
        expect(screen.getByText('a'.repeat(64))).toBeVisible();
        expect(container.querySelector('a')).toBeNull();
        expect(screen.queryByRole('button', { name: 'pages.fileRecovery.export' })).toBeNull();
        for (const result of ['outcome_unknown', 'definitely_not_started', 'changed_but_unverified']) {
            rerender(<AssistantCommandResult text={JSON.stringify({ result, output })} />);
            expect(screen.queryByRole('button', { name: /fileReceipt.title.*fileReceipt.verified/ })).toBeNull();
        }
    });
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
