import { render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { CommandConfirmationCard, validCommandReview, type CommandReview } from './device-assistant-command';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const review: CommandReview = {
    command: 'du -d 1 "a directory" | sort -n\nprintf "done\\n"',
    shell: 'bash', cwd: null, targetDeviceId: 'device-1', targetSessionId: 'connection:session',
    timeoutMs: 10000, maxStdoutBytes: 65536, maxStderrBytes: 65536,
    executionBasis: 'owner_blocklist_only', oneShot: true,
};

describe('exact command confirmation', () => {
    it('requires an intact one-shot command review before approval', () => {
        expect(validCommandReview(review)).toBe(true);
        for (const invalid of [null, undefined, {}, { command: 123 }, { ...review, command: ' ' },
            { ...review, targetSessionId: '' }, { ...review, oneShot: false },
            { ...review, timeoutMs: 0 }, { ...review, maxStdoutBytes: NaN },
            { ...review, maxStderrBytes: -1 }, { ...review, executionBasis: 'unknown' }]) {
            expect(validCommandReview(invalid)).toBe(false);
        }
    });

    it('preserves the complete multiline script and exposes scope and risk', () => {
        const value = { ...review, command: `${review.command}\n${'# full script\n'.repeat(1000)}` };
        const { container } = render(<CommandConfirmationCard value={value} />);
        expect(container.querySelector('pre')?.textContent).toBe(value.command);
        expect(container.textContent).toContain(value.targetSessionId);
        expect(container.textContent).toContain('commandFreeformWarning');
        expect(container.textContent).toContain('commandDefaultCwd');
    });
});
