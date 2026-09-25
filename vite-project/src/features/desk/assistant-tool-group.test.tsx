import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantToolGroup } from './assistant-tool-group';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('assistant tool activity summary', () => {
    it('describes a returned historical result without implying a failed operation', () => {
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'inspect_desktop_ui', status: 'returned',
                argumentsJson: '{}', output: '{"ReadContext":{}}' }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.workspace.toolGroupReturned')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });

    it('keeps a genuinely unknown action result separate from a successful tool call', () => {
        const output = JSON.stringify({ work_id: 'work', action_request_id: 'call-1',
            execution_generation: 'generation', result: 'outcome_unknown', facts: [], output: null });
        render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'execute_ui_actions', status: 'ok',
                argumentsJson: '{}', output }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('pages.aiAssistant.workspace.toolGroupOutcomeUnknown')).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.workspace.toolGroupSucceeded')).toBeNull();
    });

    it('shows a readable failure reason without exposing an opaque JSON receipt while folded', () => {
        const { container } = render(<AssistantToolGroup
            messages={[{ id: 'call', role: 'tool_call', toolCallId: 'call-1', text: '' }]}
            tools={[{ callId: 'call-1', name: 'execute_ui_actions', status: 'failed',
                argumentsJson: '{}', output: JSON.stringify({ work_id: '7bca7a6b-75fa-481d-8bc2-e2d026477925',
                    error: { message: 'Window closed before step 2' } }) }]}
            renderMessage={() => <div>Details</div>} />);
        expect(screen.getByText('Window closed before step 2')).toBeTruthy();
        expect(screen.queryByText(/7bca7a6b/)).toBeNull();
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state')).toBe('closed');
    });
});
