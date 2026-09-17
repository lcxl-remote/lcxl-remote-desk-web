import { render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { LaunchConfirmationCard, validLaunchReview, type LaunchReview } from './device-assistant-launch';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const review: LaunchReview = {
    target: { kind: 'executable', value: 'C:\\Apps\\example.exe' },
    resolvedTarget: 'C:\\Apps\\example.exe', args: ['', 'two words', '<script>literal</script>'],
    cwd: 'C:\\Work', runAsAdmin: false, targetDeviceId: 'device', targetSessionId: 'session', oneShot: true,
};
describe('application launch confirmation', () => {
    it('rejects missing identity, privilege and argument information', () => {
        expect(validLaunchReview(review)).toBe(true);
        for (const invalid of [null, {}, { ...review, args: 'one string' },
            { ...review, runAsAdmin: undefined }, { ...review, targetSessionId: '' },
            { ...review, target: { kind: 'url', value: 'https://example.test' } },
            { ...review, oneShot: false }, { ...review, args: [null] }]) {
            expect(validLaunchReview(invalid)).toBe(false);
        }
    });
    it('preserves argument boundaries and shows administrator and lifetime semantics', () => {
        const { container } = render(<LaunchConfirmationCard value={{ ...review, runAsAdmin: true }} />);
        expect(container.querySelector('[data-testid="launch-arguments"]')?.textContent).toBe(JSON.stringify(review.args, null, 2));
        expect(container.querySelector('script')).toBeNull();
        expect(container.textContent).toContain('launchAdminWarning');
        expect(container.textContent).toContain('launchLifetime');
        expect(container.textContent).toContain(review.resolvedTarget);
    });
});
