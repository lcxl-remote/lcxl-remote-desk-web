import { fireEvent, render, screen, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantContextMeter, contextBudgetShare, contextMeterValues, contextRequestBudget } from './assistant-context-meter';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string, args?: { percent?: number; value?: string }) => `${key}${args?.value ?? args?.percent ?? ''}` }) }));

describe('compression headroom meter', () => {
    it('shows request costs as shares of the full budget without adding them to history usage', async () => {
        render(<AssistantContextMeter usage={{ usedBytes: 360, limitBytes: 720, strategy: 'window',
            requestBudget: { totalBytes: 1000, systemPromptBytes: 80, toolDefinitionsBytes: 160, otherOverheadBytes: 40 },
            breakdown: { messagesBytes: 180, toolsBytes: 120, replayBytes: 40, projectedBytes: 20 } }} draft="" />);
        expect(screen.getByRole('button')).toHaveAttribute('aria-label', expect.stringContaining('percent50'));
        fireEvent.click(screen.getByRole('button'));
        const panel = await screen.findByRole('dialog');
        const request = within(within(panel).getByRole('region', { name: 'pages.aiAssistant.contextMeter.requestTitle' }));
        expect(request.getByText('pages.aiAssistant.contextMeter.request.systemPromptBytes').nextElementSibling).toHaveTextContent('bytes80');
        expect(request.getByText('pages.aiAssistant.contextMeter.request.systemPromptBytes').nextElementSibling).toHaveTextContent('share8');
        expect(request.getByText('pages.aiAssistant.contextMeter.request.toolDefinitionsBytes').nextElementSibling).toHaveTextContent('share16');
        expect(request.getByText('pages.aiAssistant.contextMeter.request.otherOverheadBytes').nextElementSibling).toHaveTextContent('share4');
        expect(within(panel).getByText('pages.aiAssistant.contextMeter.used').nextElementSibling).toHaveTextContent('bytes360');
        expect(within(panel).getByText('pages.aiAssistant.contextMeter.remaining').nextElementSibling).toHaveTextContent('bytes360');
    });
    it('shows unmeasured request costs as unknown while keeping the valid history meter', async () => {
        render(<AssistantContextMeter usage={{ usedBytes: 250, limitBytes: 1000, strategy: 'window', requestBudget: null }} draft="" />);
        fireEvent.click(screen.getByRole('button'));
        const panel = await screen.findByRole('dialog');
        expect(within(panel).getByText('pages.aiAssistant.contextMeter.request.systemPromptBytes').nextElementSibling).toHaveTextContent('—');
        expect(within(panel).getByText('pages.aiAssistant.contextMeter.request.toolDefinitionsBytes').nextElementSibling).toHaveTextContent('—');
        expect(within(panel).getByText('pages.aiAssistant.contextMeter.requestUnknown')).toBeInTheDocument();
        expect(screen.getByRole('button')).toHaveAttribute('aria-label', expect.stringContaining('percent25'));
    });
    it('rejects inconsistent or unsafe request metadata without invalidating history usage', () => {
        const usage = { usedBytes: 100, limitBytes: 720, strategy: 'window',
            requestBudget: { totalBytes: 1000, systemPromptBytes: 80, toolDefinitionsBytes: 160, otherOverheadBytes: 40 } };
        expect(contextRequestBudget(usage)).toEqual(usage.requestBudget);
        for (const requestBudget of [
            { ...usage.requestBudget, otherOverheadBytes: 39 },
            { ...usage.requestBudget, systemPromptBytes: -1 },
            { ...usage.requestBudget, totalBytes: Number.MAX_SAFE_INTEGER + 1 },
            { ...usage.requestBudget, toolDefinitionsBytes: NaN },
        ]) {
            expect(contextRequestBudget({ ...usage, requestBudget })).toBeNull();
            expect(contextMeterValues({ ...usage, requestBudget }, '')?.remaining).toBe(620);
        }
    });
    it('distinguishes small nonzero shares from zero and avoids premature rounding to 100%', () => {
        expect(contextBudgetShare(0, 1000)).toBe(0);
        expect(contextBudgetShare(2, 16384)).toBe('small');
        expect(contextBudgetShare(1, 1000)).toBe(0.1);
        expect(contextBudgetShare(9999, 10000)).toBe(99.9);
        expect(contextBudgetShare(10000, 10000)).toBe(100);
        expect(contextBudgetShare(-1, 1000)).toBeNull();
        expect(contextBudgetShare(1, 0)).toBeNull();
        expect(contextBudgetShare(1001, 1000)).toBeNull();
    });
    it('renders the small-share label and clears request details when switching conversations', async () => {
        const usage = { usedBytes: 100, limitBytes: 16382, strategy: 'window',
            requestBudget: { totalBytes: 16384, systemPromptBytes: 0, toolDefinitionsBytes: 2, otherOverheadBytes: 0 } };
        const view = render(<AssistantContextMeter usage={usage} draft="" />);
        fireEvent.click(screen.getByRole('button'));
        await screen.findByRole('dialog');
        expect(screen.getByText('pages.aiAssistant.contextMeter.shareSmall')).toBeInTheDocument();
        view.rerender(<AssistantContextMeter usage={null} draft="" />);
        expect(screen.queryByText('pages.aiAssistant.contextMeter.shareSmall')).toBeNull();
        expect(screen.queryByText('pages.aiAssistant.contextMeter.request.systemPromptBytes')).toBeNull();
    });
    it('shows cost categories without adding replay again to total occupancy', async () => {
        render(<AssistantContextMeter usage={{ usedBytes: 250, limitBytes: 1000, strategy: 'checkpoint_summary',
            breakdown: { messagesBytes: 50, toolsBytes: 70, replayBytes: 120, projectedBytes: 10 } }} draft="" />);
        expect(screen.getByRole('button')).toHaveAttribute('aria-label', expect.stringContaining('percent25'));
        fireEvent.click(screen.getByRole('button'));
        const panel = await screen.findByRole('dialog');
        expect(panel).toHaveTextContent('breakdown.replayBytes');
        expect(panel).toHaveTextContent('bytes120');
        expect(panel).toHaveTextContent('bytes250');
    });
    it('reveals budget details on tap without sending a message', async () => {
        render(<AssistantContextMeter usage={{ usedBytes: 250, limitBytes: 1000, strategy: 'checkpoint_summary' }} draft="" />);
        fireEvent.click(screen.getByRole('button'));
        const tooltip = await screen.findByRole('dialog');
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
