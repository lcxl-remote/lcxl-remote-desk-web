import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/react';

import { presetRange } from './usage-range-picker';

// i18n: real en-US locale so button labels/captions match production copy.
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()));

import { UsageRangePicker } from './usage-range-picker';

// A fixed instant so the preset arithmetic is deterministic. 2026-06-24T12:00Z.
const NOW = new Date('2026-06-24T12:00:00.000Z');

describe('presetRange', () => {
    it('24h looks back exactly one day and ends at now', () => {
        const r = presetRange('24h', NOW);
        expect(r.to).toBe('2026-06-24T12:00:00.000Z');
        expect(r.from).toBe('2026-06-23T12:00:00.000Z');
    });

    it('7d looks back seven days', () => {
        const r = presetRange('7d', NOW);
        expect(r.from).toBe('2026-06-17T12:00:00.000Z');
        expect(r.to).toBe('2026-06-24T12:00:00.000Z');
    });

    it('30d looks back thirty days (wide enough to force day granularity server-side)', () => {
        const r = presetRange('30d', NOW);
        expect(r.from).toBe('2026-05-25T12:00:00.000Z');
        expect(r.to).toBe('2026-06-24T12:00:00.000Z');
    });

    it('does not mutate the passed-in now', () => {
        const now = new Date('2026-06-24T12:00:00.000Z');
        presetRange('30d', now);
        expect(now.toISOString()).toBe('2026-06-24T12:00:00.000Z');
    });
});

describe('UsageRangePicker', () => {
    it('emits a ~7-day window when the 7d preset is clicked', () => {
        const onChange = vi.fn();
        const { getByText } = render(<UsageRangePicker value={{}} onChange={onChange} />);
        fireEvent.click(getByText('Last 7 days'));
        expect(onChange).toHaveBeenCalledTimes(1);
        const arg = onChange.mock.calls[0][0] as { from?: string; to?: string };
        const spanMs = new Date(arg.to!).getTime() - new Date(arg.from!).getTime();
        expect(spanMs).toBe(7 * 24 * 60 * 60 * 1000);
    });

    it('reveals custom datetime inputs when Custom is chosen', () => {
        const { getByText, container } = render(
            <UsageRangePicker value={{}} onChange={vi.fn()} />,
        );
        expect(container.querySelectorAll('input[type="datetime-local"]').length).toBe(0);
        fireEvent.click(getByText('Custom'));
        expect(container.querySelectorAll('input[type="datetime-local"]').length).toBe(2);
    });

    it('labels a day-granularity effective range as UTC', () => {
        const { container } = render(
            <UsageRangePicker
                value={{}}
                onChange={vi.fn()}
                effective={{
                    from: '2026-05-25T00:00:00.000Z',
                    to: '2026-06-24T00:00:00.000Z',
                    granularity: 'day',
                }}
            />,
        );
        expect(container.textContent).toContain('UTC calendar day');
    });

    it('omits the UTC note for an hour-granularity effective range', () => {
        const { container } = render(
            <UsageRangePicker
                value={{}}
                onChange={vi.fn()}
                effective={{
                    from: '2026-06-23T12:00:00.000Z',
                    to: '2026-06-24T12:00:00.000Z',
                    granularity: 'hour',
                }}
            />,
        );
        expect(container.textContent).not.toContain('UTC calendar day');
    });
});
