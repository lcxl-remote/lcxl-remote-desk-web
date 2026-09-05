import { act, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantContextMeter, contextMeterValues } from './assistant-context-meter';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string, args?: { percent?: number; value?: string }) => `${key}${args?.value ?? args?.percent ?? ''}` }) }));

describe('compression headroom meter', () => {
    it('reveals budget details on keyboard focus without sending a message', async () => {
        render(<AssistantContextMeter usage={{ usedBytes: 250, limitBytes: 1000, strategy: 'checkpoint_summary' }} draft="" />);
        act(() => screen.getByRole('button').focus());
        const tooltip = await screen.findByRole('tooltip');
        expect(tooltip.textContent).toContain('limit.checkpoint_summary');
        expect(tooltip.textContent).toContain('bytes1,000');
        expect(tooltip.textContent).toContain('bytes750');
    });
    it('uses the effective history threshold and estimates UTF-8 JSON framing, not characters', () => {
        const value = contextMeterValues({ usedBytes: 250, limitBytes: 1000, strategy: 'checkpoint_summary' }, '你好');
        expect(value).toMatchObject({ percent: 25, remaining: 750 });
        expect(value?.draftBytes).toBe(new TextEncoder().encode(JSON.stringify({ role: 'user', text: '你好' })).length);
    });
    it('shows zero for a measured empty window, and no usage for missing or invalid data', () => {
        expect(contextMeterValues({ usedBytes: 0, limitBytes: 1000, strategy: 'window' }, '')).toEqual({ percent: 0, remaining: 1000, draftBytes: 0 });
        expect(contextMeterValues(null, '')).toBeNull();
        expect(contextMeterValues({ usedBytes: -1, limitBytes: 1000, strategy: 'window' }, '')).toBeNull();
        expect(contextMeterValues({ usedBytes: 1, limitBytes: 0, strategy: 'window' }, '')).toBeNull();
        expect(contextMeterValues({ usedBytes: 1, limitBytes: 1000, strategy: 'future' }, '')).toBeNull();
    });
    it('clamps overflow and does not round to 100 before the threshold', () => {
        expect(contextMeterValues({ usedBytes: 999, limitBytes: 1000, strategy: 'window' }, '')?.percent).toBe(99);
        expect(contextMeterValues({ usedBytes: 1200, limitBytes: 1000, strategy: 'window' }, '')).toMatchObject({ percent: 100, remaining: 0 });
    });
    it('provides a focusable ring and clears its displayed usage on conversation reset', () => {
        const { rerender } = render(<AssistantContextMeter usage={{ usedBytes: 500, limitBytes: 1000, strategy: 'window' }} draft="" />);
        expect(screen.getByRole('button').getAttribute('aria-label')).toContain('percent50');
        rerender(<AssistantContextMeter usage={null} draft="" />);
        expect(screen.getByRole('button').getAttribute('aria-label')).toContain('unknown');
        expect(screen.queryByText('50%')).toBeNull();
    });
});
