import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { GrantRequestItemDto, PermissionRequestDto, WaylandOutputConfirmationDto } from '@/services/types';
import { OutputConfirmationCard, outputApprovalBlocked, validOutputReview } from './ai-assistant-output-confirmation';
import { AssistantPermissionRequest } from './assistant-permission-request';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const review: WaylandOutputConfirmationDto = {
    screen: { display: 'output', width: 200, height: 100, dpi_x: 96, dpi_y: 96 },
    step: { kind: 'click', params: { x: 10, y: 20, button: 'primary' } }, wholeOutput: true, oneShot: true,
};
const item = (overrides: Partial<GrantRequestItemDto> = {}): GrantRequestItemDto => ({
    itemId: 'step', providerId: 'desktop.output.input', toolName: 'execute_wayland_output_input',
    expectedEffect: 'input_fallback', resourceScope: [`wayland_output:${'a'.repeat(64)}`],
    operationScope: ['wayland_output_input:exact_step'], exportDestinations: [], suggestedTtlSeconds: 60,
    suggestedMaxUses: 1, reason: 'test', waylandOutputConfirmation: review, ...overrides,
});
const request = (entry: GrantRequestItemDto): PermissionRequestDto => ({
    schemaVersion: 1, requestId: 'output', inputRevision: 1, state: 'pending', createdAt: 'now', items: [entry],
});

describe('whole-output exact approval', () => {
    it('blocks missing or mismatched authority projections', () => {
        expect(outputApprovalBlocked(item())).toBe(false);
        for (const overrides of [
            { waylandOutputConfirmation: null }, { suggestedMaxUses: 2 }, { providerId: 'other' },
            { resourceScope: ['application:123'] }, { operationScope: ['input'] },
            { waylandOutputConfirmation: { ...review, wholeOutput: false } },
            { waylandOutputConfirmation: { ...review, oneShot: false } },
        ]) expect(outputApprovalBlocked(item(overrides))).toBe(true);
    });
    it('rejects unknown, invisible and out-of-bounds steps', () => {
        for (const step of [
            { kind: 'click', params: { x: 200, y: 20, button: 'primary' } },
            { kind: 'click', params: { x: 10, y: 20, button: 'middle' } },
            { kind: 'key_press', params: { key: 'launch_terminal' } },
            { kind: 'key_press', params: { key: 'enter', text: 'hidden' } },
            { kind: 'scroll', params: { horizontal: 0, vertical: 0 } },
            { kind: 'scroll', params: { horizontal: 0, vertical: 1201 } },
            { kind: 'type_text', params: { text: 'line\nnext' } },
            { kind: 'type_text', params: { text: '🙂'.repeat(65) } },
            { kind: 'macro', params: {} },
        ]) expect(validOutputReview({ ...review, step } as WaylandOutputConfirmationDto)).toBe(false);
        expect(validOutputReview({ ...review, step: { kind: 'type_text', params: { text: '🙂'.repeat(64) } } })).toBe(true);
    });
    it('shows literal text and the entire-output warning', () => {
        const value: WaylandOutputConfirmationDto = { ...review, step: { kind: 'type_text', params: { text: '<script>literal</script>' } } };
        const { container } = render(<OutputConfirmationCard value={value} />);
        expect(container.querySelector('script')).toBeNull();
        expect(container.querySelector('pre')?.textContent).toBe('<script>literal</script>');
        expect(screen.getByText('pages.aiAssistant.outputConfirmScope')).toBeInTheDocument();
    });
    it('submits one use and denies a request without a review', () => {
        const onDecide = vi.fn().mockResolvedValue(true);
        const { rerender } = render(<AssistantPermissionRequest request={request(item())} canDecide onDecide={onDecide} />);
        expect(screen.getByTestId('wayland-output-confirmation')).toBeInTheDocument();
        const uses = screen.getAllByRole('spinbutton').find(input => (input as HTMLInputElement).disabled);
        expect(uses).toHaveValue(1);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.permissionSubmitSelection' }));
        expect(onDecide.mock.calls[0][1][0]).toMatchObject({ decision: 'approve', max_uses: 1 });
        rerender(<AssistantPermissionRequest request={request(item({ waylandOutputConfirmation: null }))} canDecide onDecide={onDecide} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.permissionSubmitSelection' }));
        expect(onDecide.mock.calls[1][1][0]).toMatchObject({ decision: 'deny' });
    });
});
