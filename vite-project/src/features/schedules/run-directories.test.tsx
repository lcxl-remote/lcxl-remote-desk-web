import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { RunDirectories } from './run-directories';
import { AssistantFileScope } from '../desk/assistant-file-scope';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
type Props = Parameters<typeof RunDirectories>[0];
const baseTime = Date.parse('2026-09-14T12:00:00Z');
const approve = () => screen.getByRole('button', { name: 'pages.aiAssistant.directories.approve' });
const reject = () => screen.getByRole('button', { name: 'pages.aiAssistant.directories.reject' });
function props(path: string): Props {
    return { scheduleId: 'task-1', runId: 'run-1', connected: true, loading: false,
        snapshot: { sessionId: 'session-1', requestId: 'run-1', fileScope: { revision: 7, directories: [{
            requestId: 'directory-1', canonicalPath: path, purpose: 'Read inputs', source: 'model_proposal',
            state: 'pending', referenceExpiresAt: new Date(baseTime + 1000).toISOString(),
        }] } },
        client: { request: vi.fn().mockResolvedValue({ result: 'task', task: { schedule_id: 'task-1' } }) },
        onReload: vi.fn().mockResolvedValue(undefined),
    };
}
beforeEach(() => { vi.useFakeTimers(); vi.setSystemTime(baseTime); });
afterEach(() => { cleanup(); vi.useRealTimers(); });

describe.each(['C:\\用户资料\\季度 报告', '/Users/owner/季度 报告'])('directory approval: %s', path => {
    it.each(['scheduled', 'conversation'])('%s remains approvable without a directory time limit', async surface => {
        const input = props(path);
        const update = vi.fn(() => true);
        if (surface === 'scheduled') render(<RunDirectories {...input} />);
        else render(<AssistantFileScope scope={input.snapshot.fileScope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        expect(approve()).toBeEnabled();
        await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
        expect(approve()).toBeEnabled();
        expect(screen.queryByText('pages.aiAssistant.directories.expired')).toBeNull();
        await act(async () => { fireEvent.click(approve()); });
        if (surface === 'scheduled') expect(input.client.request).toHaveBeenCalledWith(expect.objectContaining({ approve: true, expected_scope_revision: 7 }));
        else expect(update).toHaveBeenCalledWith(expect.objectContaining({ approve: true, expected_revision: 7 }), expect.any(String));
    });

    it('does not expire a directory after resuming a suspended page', () => {
        const input = props(path);
        const view = render(<RunDirectories {...input} />);
        vi.setSystemTime(baseTime + 2000);
        fireEvent.focus(window);
        expect(approve()).toBeEnabled();
        const renewed = props(path);
        renewed.snapshot.fileScope.directories[0].referenceExpiresAt = new Date(baseTime + 5000).toISOString();
        view.rerender(<RunDirectories {...renewed} />);
        expect(approve()).toBeEnabled();
    });

    it('binds one approval to its run and revision and ignores its response after switching runs', async () => {
        const input = props(path);
        let resolve!: (value: unknown) => void;
        input.client.request = vi.fn(() => new Promise(done => { resolve = done; })) as Props['client']['request'];
        const view = render(<RunDirectories {...input} />);
        fireEvent.click(approve()); fireEvent.click(approve());
        expect(input.client.request).toHaveBeenCalledTimes(1);
        expect(input.client.request).toHaveBeenCalledWith(expect.objectContaining({ operation: 'decide_run_directory',
            schedule_id: 'task-1', run_id: 'run-1', directory_request_id: 'directory-1', approve: true, expected_scope_revision: 7 }));
        view.rerender(<RunDirectories {...input} runId="run-2" snapshot={{ ...input.snapshot, requestId: 'run-2' }} />);
        await act(async () => { resolve({ result: 'task', task: { schedule_id: 'task-1' } }); });
        expect(input.onReload).not.toHaveBeenCalled();
        expect(screen.queryByText('schedules.approval.recorded')).not.toBeInTheDocument();
    });

    it('keeps revocation available after time passes', async () => {
        const input = props(path);
        input.snapshot.fileScope.directories[0].state = 'approved';
        render(<RunDirectories {...input} />);
        await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
        expect(screen.getByText('schedules.directoryState.approved')).toBeInTheDocument();
        const remove = screen.getByRole('button', { name: 'pages.aiAssistant.directories.remove' });
        expect(remove).toBeEnabled();
        await act(async () => { fireEvent.click(remove); });
        expect(input.client.request).toHaveBeenCalledWith(expect.objectContaining({ operation: 'revoke_run_directory', expected_scope_revision: 7 }));
    });

    it('accepts a timeless directory and does not replay an uncertain rejection', async () => {
        const input = props(path);
        input.snapshot.fileScope.directories[0].referenceExpiresAt = '';
        input.client.request = vi.fn().mockRejectedValue(new Error('lost response'));
        render(<RunDirectories {...input} />);
        expect(approve()).toBeEnabled();
        await act(async () => { fireEvent.click(reject()); });
        expect(input.client.request).toHaveBeenCalledTimes(1);
        expect(input.onReload).toHaveBeenCalledTimes(1);
        await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
        expect(input.client.request).toHaveBeenCalledTimes(1);
    });
});
