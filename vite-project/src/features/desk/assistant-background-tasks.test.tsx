import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantBackgroundTasks } from './assistant-background-tasks';
import { AssistantComposerTools } from './assistant-composer-tools';
import type { CommandTaskDto } from '@/services/types';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const command: CommandTaskDto = { taskId: 'command-1', callId: 'call-1', executionGeneration: 'generation-1',
    state: 'running', updatedAt: '2026-09-08T10:00:00Z', result: null, resultTruncated: false };
const base = { open: true, onOpenChange: vi.fn(), commands: [], providers: [], tools: [],
    connected: true, canCancelProvider: true, cancelling: null, onCancel: vi.fn().mockResolvedValue(undefined) };

describe('assistant background tasks', () => {
    it('offers a named task entry even without tasks and never submits a message', () => {
        const onTasks = vi.fn(); const onSubmit = vi.fn();
        render(<form onSubmit={onSubmit}><AssistantComposerTools meter={null} onDetails={vi.fn()}
            onPermissionHistory={vi.fn()} onTasks={onTasks} runningTaskCount={2} /></form>);
        const button = screen.getByRole('button', { name: 'pages.deviceAssistant.tasks.title' });
        expect(button.textContent).toContain('(2)');
        fireEvent.click(button);
        expect(onTasks).toHaveBeenCalledOnce(); expect(onSubmit).not.toHaveBeenCalled();
    });
    it('shows an empty state', () => {
        render(<AssistantBackgroundTasks {...base} />);
        expect(screen.getByText('pages.deviceAssistant.tasks.empty')).toBeTruthy();
    });
    it('keeps completed results and cancels only the selected unfinished command', async () => {
        const onCancel = vi.fn().mockResolvedValue(undefined);
        render(<AssistantBackgroundTasks {...base} onCancel={onCancel} commands={[
            { ...command, taskId: 'done', executionGeneration: 'done-generation', state: 'succeeded', result: 'saved result' }, command,
        ]} />);
        expect(screen.getByText('saved result')).toBeTruthy();
        const buttons = screen.getAllByRole('button', { name: 'pages.deviceAssistant.tasks.cancel' });
        expect(buttons).toHaveLength(1);
        fireEvent.click(buttons[0]);
        await waitFor(() => expect(onCancel).toHaveBeenCalledWith('command', 'command-1'));
        expect(await screen.findByRole('status')).toBeTruthy();
        expect(screen.getByText('pages.deviceAssistant.backgroundState.running')).toBeTruthy();
    });
    it('does not offer command cancellation while the device is offline', () => {
        render(<AssistantBackgroundTasks {...base} commands={[command]} connected={false} />);
        expect(screen.queryByRole('button', { name: 'pages.deviceAssistant.tasks.cancel' })).toBeNull();
        expect(within(screen.getByRole('dialog')).getByText('pages.deviceAssistant.tasks.offline')).toBeTruthy();
    });
});
