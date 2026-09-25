import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantToolCall } from './assistant-tool-call';
import type { AiAssistantToolActivity } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const tool: AiAssistantToolActivity = {
    callId: 'call-1', name: 'inspect_desktop_ui', status: 'running',
    argumentsJson: '{"queries":["Calendar"]}', output: null,
};

describe('tool call transcript', () => {
    it('labels a permission-paused sibling as historically skipped, not an active denial', () => {
        render(<AssistantToolCall running={false} tool={{ ...tool,
            name: 'describe_tools', status: 'failed',
            output: 'not executed: waiting for user permission decision',
        }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.skippedForPermission' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAssistant.toolCall.failure' })).toBeNull();
        fireEvent.click(screen.getByText(/describe_tools/));
        expect(screen.getByText('pages.aiAssistant.historicalPermissionSkip')).toBeTruthy();
    });

    it('labels a sibling skipped for an existing pending request the same way', () => {
        render(<AssistantToolCall running={false} tool={{ ...tool,
            name: 'describe_tools', status: 'failed',
            output: 'not executed: waiting for the existing user permission decision',
        }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.skippedForPermission' })).toBeTruthy();
    });

    it('labels the pending permission tool result as its submission-time receipt', () => {
        render(<AssistantToolCall running={false} tool={{ ...tool,
            name: 'request_permissions', status: 'ok',
            output: JSON.stringify({ status: 'pending_user_decision', request_id: 'request-1', authority: 'none', item_count: 1 }),
        }} />);
        fireEvent.click(screen.getByText(/request_permissions/));
        expect(screen.getByText('pages.aiAssistant.permissionSubmissionReceipt')).toBeTruthy();
    });

    it('shows an associated unknown file result while folded without calling it success or ordinary failure', () => {
        const output = { work_id: 'work', action_request_id: tool.callId, execution_generation: 'generation',
            result: 'outcome_unknown', facts: [{ index: 0, changed: true, verified: false }], output: null };
        const { container, rerender } = render(<AssistantToolCall running={false} tool={{ ...tool,
            name: 'create_text_file', status: 'failed', output: JSON.stringify(output) }} />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.outcomeUnknown' })).toBeTruthy();
        expect(screen.getByText(/pages.aiAssistant.toolCall.inspectBeforeRetry/)).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
        expect(screen.queryByRole('img', { name: 'pages.aiAssistant.toolCall.failure' })).toBeNull();
        rerender(<AssistantToolCall running={false} tool={{ ...tool, status: 'failed',
            output: JSON.stringify({ ...output, action_request_id: 'another-call' }) }} />);
        expect(screen.queryByRole('img', { name: 'pages.aiAssistant.toolCall.outcomeUnknown' })).toBeNull();
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.failure' })).toBeTruthy();
    });

    it('shows accessible status icons while folded and updates from running to success or failure', () => {
        const { container, rerender } = render(<AssistantToolCall tool={tool} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.waiting' }).querySelector('.animate-spin')).toBeTruthy();
        rerender(<AssistantToolCall tool={{ ...tool, status: 'ok', output: '' }} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.success' })).toBeTruthy();
        expect(container.querySelector('.animate-spin')).toBeNull();
        rerender(<AssistantToolCall tool={{ ...tool, status: 'failed', output: 'access denied' }} running />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.failure' })).toBeTruthy();
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(container.querySelector('pre')).toBeNull();
        rerender(<AssistantToolCall tool={tool} running={false} />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.missing' })).toBeTruthy();
        expect(container.querySelector('.animate-spin')).toBeNull();
        expect(screen.queryByRole('img', { name: 'pages.aiAssistant.toolCall.success' })).toBeNull();
    });

    it('labels a persisted result as returned without claiming tool success', () => {
        render(<AssistantToolCall tool={{ ...tool, status: 'returned', output: 'observed' }} running={false} />);
        expect(screen.getByRole('img', { name: 'pages.aiAssistant.toolCall.returned' })).toBeTruthy();
        expect(screen.queryByRole('img', { name: 'pages.aiAssistant.toolCall.success' })).toBeNull();
    });

    it('starts folded, lazily renders payloads, and keeps expansion when output arrives', async () => {
        const { container, rerender } = render(<AssistantToolCall tool={tool} running />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(container.querySelector('pre')).toBeNull();
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await waitFor(() => expect(container.querySelectorAll('pre')).toHaveLength(2));
        expect(container.querySelector('pre')?.textContent).toBe(JSON.stringify(JSON.parse(tool.argumentsJson), null, 2));
        expect(screen.getByText('pages.aiAssistant.toolCall.waiting')).toBeTruthy();
        rerender(<AssistantToolCall tool={{ ...tool, status: 'failed', output: 'tool error: access denied <script>alert(1)</script>' }} running={false} />);
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(true);
        expect(screen.getByText(/tool error: access denied/)).toBeTruthy();
        expect(container.querySelector('script')).toBeNull();
    });

    it('distinguishes missing results from empty output after the conversation stops', async () => {
        const { rerender } = render(<AssistantToolCall tool={tool} running={false} />);
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await screen.findByText('pages.aiAssistant.toolCall.missing');
        rerender(<AssistantToolCall tool={{ ...tool, status: 'ok', output: '' }} running={false} />);
        expect(screen.getByText('pages.aiAssistant.toolCall.empty')).toBeTruthy();
    });
    it('shows the first failed batch step while keeping full inputs folded', () => {
        const { container } = render(<AssistantToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', argumentsJson: '{"steps":[{"action":{"kind":"invoke"}}]}', output: '{"status":"stopped_on_error","failed_step_number":2,"error":{"message":"window closed"}}' }} running={false} />);
        expect(screen.getByText(/pages.aiAssistant.toolCall.batchFailed/)).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
    });

    it.each([
        [0, 'no_effect', 'batchNotStarted'],
        [1, 'no_effect', 'batchPartiallyDispatched'],
        [0, 'may_have_effect', 'batchOutcomeUnknown'],
        [1, 'may_have_effect', 'batchOutcomeUnknown'],
    ])('shows the native execution stage for %s completed dispatches with %s', (count, effect, key) => {
        const output = JSON.stringify({ status: 'stopped_on_error', failed_step_number: Number(count) + 1, completed_steps: count, effect });
        const { container, rerender } = render(<AssistantToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAssistant.toolCall.${key}`))).toBeTruthy();
        expect(container.querySelector('pre')).toBeNull();
        rerender(<AssistantToolCall tool={{ ...tool, name: 'send_background_input', status: 'failed', output: JSON.stringify({ message: output }) }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAssistant.toolCall.${key}`))).toBeTruthy();
    });

    it('does not infer non-execution from an inconsistent receipt', () => {
        render(<AssistantToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output: JSON.stringify({ status: 'stopped_on_error', failed_step_number: 2, completed_steps: 0, effect: 'no_effect' }) }} running={false} />);
        expect(screen.getByText(/pages.aiAssistant.toolCall.batchFailed/)).toBeTruthy();
        expect(screen.queryByText(/pages.aiAssistant.toolCall.batchNotStarted/)).toBeNull();
    });

    it.each([
        ['definitely_not_started', 'batchNotStarted'],
        ['outcome_unknown', 'batchOutcomeUnknown'],
    ])('uses the typed %s result for an executor error without a step receipt', (result, key) => {
        render(<AssistantToolCall tool={{ ...tool, name: 'execute_ui_actions', status: 'failed', output: JSON.stringify({ result, message: 'native UI request stopped' }) }} running={false} />);
        expect(screen.getByText(new RegExp(`pages.aiAssistant.toolCall.${key}`))).toBeTruthy();
        expect(screen.queryByText(/pages.aiAssistant.toolCall.batchFailed/)).toBeNull();
    });

});
