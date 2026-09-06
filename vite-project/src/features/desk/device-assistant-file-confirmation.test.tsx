import { render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { TextFileConfirmationCard, validTextFileReview, fileApprovalBlocked } from './device-assistant-file-confirmation';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const review = {
    fileName: 'notes.txt', fileResultCallId: 'call-real', expectedSha256: 'a'.repeat(64),
    operation: 'update' as const, change: { kind: 'replace_once' as const, before: '版本=1\n', after: '版本=2\n' },
    oneShot: true, recoverable: true,
};

describe('exact text file approval', () => {
    it('blocks missing, malformed and mismatched operation reviews', () => {
        expect(fileApprovalBlocked({ toolName: 'update_text_file', textFileConfirmation: review })).toBe(false);
        expect(fileApprovalBlocked({ toolName: 'delete_text_file', textFileConfirmation: review })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'update_text_file' })).toBe(true);
        expect(fileApprovalBlocked({ toolName: 'read_selected_text_file' })).toBe(false);
        for (const invalid of [null, {}, { ...review, expectedSha256: 'bad' }, { ...review, recoverable: false },
            { ...review, oneShot: false }, { ...review, change: null }, { ...review, change: { kind: 'replace_once', before: '' } }]) {
            expect(validTextFileReview(invalid)).toBe(false);
        }
    });
    it('shows literal before/after text and recoverable deletion without rendering HTML', () => {
        const value = { ...review, change: { ...review.change, after: '<script>not executable</script>\n' } };
        const { container, rerender } = render(<TextFileConfirmationCard value={value} />);
        expect([...container.querySelectorAll('pre')].map(node => node.textContent)).toEqual([value.change.before, value.change.after]);
        expect(container.querySelector('script')).toBeNull();
        expect(container.textContent).toContain('notes.txt');
        rerender(<TextFileConfirmationCard value={{ ...review, operation: 'delete', change: null }} />);
        expect(container.textContent).toContain('fileConfirmRecoverable');
        expect(container.querySelector('pre')).toBeNull();
    });
    it('accepts an explicit empty full replacement, never an omitted body', () => {
        expect(validTextFileReview({ ...review, change: { kind: 'replace_all', content_utf8: '' } })).toBe(true);
        expect(validTextFileReview({ ...review, change: { kind: 'replace_all' } })).toBe(false);
    });
});
