import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantToolCall } from './assistant-tool-call';
import type { DeviceAssistantToolActivity } from './use-device-assistant-chat';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const tool: DeviceAssistantToolActivity = {
    callId: 'call-1', name: 'inspect_desktop_ui', status: 'running',
    argumentsJson: '{"query":{"name":"Calendar"}}', output: null,
};

describe('tool call transcript', () => {
    it('starts folded, lazily renders payloads, and keeps expansion when output arrives', async () => {
        const { container, rerender } = render(<AssistantToolCall tool={tool} running />);
        expect(container.querySelector('details')?.open).toBe(false);
        expect(container.querySelector('pre')).toBeNull();
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await waitFor(() => expect(container.querySelectorAll('pre')).toHaveLength(2));
        expect(container.querySelector('pre')?.textContent).toBe(JSON.stringify(JSON.parse(tool.argumentsJson), null, 2));
        expect(screen.getByText('pages.deviceAssistant.toolCall.waiting')).toBeTruthy();
        rerender(<AssistantToolCall tool={{ ...tool, status: 'failed', output: 'tool error: access denied <script>alert(1)</script>' }} running={false} />);
        expect(container.querySelector('details')?.open).toBe(true);
        expect(screen.getByText(/tool error: access denied/)).toBeTruthy();
        expect(container.querySelector('script')).toBeNull();
    });

    it('distinguishes missing results from empty output after the conversation stops', async () => {
        const { rerender } = render(<AssistantToolCall tool={tool} running={false} />);
        fireEvent.click(screen.getByText(/inspect_desktop_ui/));
        await screen.findByText('pages.deviceAssistant.toolCall.missing');
        rerender(<AssistantToolCall tool={{ ...tool, status: 'ok', output: '' }} running={false} />);
        expect(screen.getByText('pages.deviceAssistant.toolCall.empty')).toBeTruthy();
    });
});
