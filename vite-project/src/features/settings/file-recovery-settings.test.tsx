import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { RestResponseError } from '@/lib/kubb-client';
import { FileRecoverySettings } from './file-recovery-settings';
const api = vi.hoisted(() => ({ query: vi.fn(), save: vi.fn(), cleanup: vi.fn(), download: vi.fn(), remote: vi.fn(), discard: vi.fn(), clock: vi.fn() }));
vi.mock('@/services/clients', () => ({ queryLocalFileRecovery: api.query, updateLocalFileRecoveryPolicy: api.save, retryLocalFileRecoveryCleanup: api.cleanup, exportLocalFileRecovery: api.download, manageDeviceFileRecovery: api.remote, discardLocalFileRecovery: api.discard, confirmLocalFileRecoveryClock: api.clock }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
beforeEach(() => {
    vi.clearAllMocks();
    api.query.mockResolvedValue({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0, records: [], next_cursor: null } });
    api.save.mockResolvedValue({ data: { retention_days: 1, max_bytes: 104857600 } });
});
it('drops a late page after switching devices and starts with a fresh authority', async () => {
    let resolveOld!: (value: unknown) => void;
    api.remote.mockImplementationOnce(() => new Promise(resolve => { resolveOld = resolve; }));
    const view = render(<FileRecoverySettings target={{ connection: 'old', device_id: 'old-device' }} />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    view.rerender(<FileRecoverySettings target={{ connection: 'new', device_id: 'new-device' }} />);
    await act(async () => resolveOld({ data: { authority: 'old-authority', os_user: 'old-user', outcome: {
        kind: 'page', page: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0,
            records: [{ recovery_id: 'old-backup', file_name: 'old-private.txt' }], next_cursor: 'old-cursor' },
    } } }));
    expect(screen.queryByText('old-private.txt')).not.toBeInTheDocument();
    api.remote.mockResolvedValueOnce({ data: { authority: 'new-authority', os_user: 'new-user', outcome: {
        kind: 'page', page: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0, records: [], next_cursor: null },
    } } });
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    await waitFor(() => expect(api.remote).toHaveBeenLastCalledWith({ connection: 'new', device_id: 'new-device',
        request: { expected_authority: undefined, expected_os_user: undefined, command: { operation: 'query', after: undefined } } }));
    await screen.findByLabelText('pages.fileRecovery.days');
});

it('does not download a late backup after the local recovery view is closed', async () => {
    let finish!: (value: Blob) => void;
    api.query.mockResolvedValueOnce({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 100, reserved_bytes: 0,
        records: [{ recovery_id: 'backup', conversation_id: 'conversation', file_name: 'notes.txt',
            created_at_unix_ms: 1000, expires_at_unix_ms: 2000, size_bytes: 100,
            material_state: 'saved', change_state: 'succeeded', cleanup_pending: false,
            cleanup_reason: null, export_available: true }], next_cursor: null } });
    api.download.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {});
    try {
        const view = render(<FileRecoverySettings />);
        fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
        fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
        fireEvent.click(await screen.findByRole('button', { name: 'pages.fileRecovery.export' }));
        expect(api.download).toHaveBeenCalledTimes(1);
        view.unmount();
        await act(async () => finish(new Blob(['archive'], { type: 'application/zip' })));
        expect(click).not.toHaveBeenCalled();
    } finally { click.mockRestore(); }
});
it('only confirms the displayed device time after explicit approval and does not repeat it on refresh failure', async () => {
    api.query.mockResolvedValueOnce({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0, records: [], next_cursor: null,
        cleanup_warning: 'clock_changed', clock_confirmation_time_unix_ms: 1234 } });
    api.clock.mockResolvedValue({ data: { pending_files: 0, unknown_outcomes: 0 } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    fireEvent.click(await screen.findByRole('button', { name: 'pages.fileRecovery.confirmClock' }));
    let dialog = await screen.findByRole('alertdialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.fileRecovery.cancel' }));
    expect(api.clock).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.confirmClock' }));
    dialog = await screen.findByRole('alertdialog');
    api.query.mockRejectedValueOnce(new Error('offline'));
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.fileRecovery.confirmClock' }));
    expect(await screen.findByRole('status')).toHaveTextContent('pages.fileRecovery.clockRefreshFailed');
    expect(api.clock).toHaveBeenCalledExactlyOnceWith({ displayed_time_unix_ms: 1234, confirmed: true });
});
it('preserves a typed cleanup failure instead of replacing it with a generic error', async () => {
    api.remote.mockResolvedValue({ data: { authority: 'authority', os_user: '501', outcome: { kind: 'unavailable', reason: 'clock_changed' } } });
    render(<FileRecoverySettings target={{ connection: 'connection' }} />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    expect(await screen.findByRole('status')).toHaveTextContent('pages.fileRecovery.error.clock_changed');
});
it('requires confirmation to discard and never repeats a successful discard when refreshing fails', async () => {
    api.query.mockResolvedValueOnce({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 100, reserved_bytes: 0,
        records: [{ recovery_id: 'backup', conversation_id: 'conversation', file_name: 'notes.txt', change_state: 'outcome_unknown', export_available: true }], next_cursor: null } });
    api.discard.mockResolvedValue({ data: { pending_files: 0, unknown_outcomes: 0 } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    fireEvent.click(await screen.findByRole('button', { name: 'pages.fileRecovery.discard' }));
    let dialog = await screen.findByRole('alertdialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.fileRecovery.cancel' }));
    expect(api.discard).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.discard' }));
    dialog = await screen.findByRole('alertdialog');
    api.query.mockRejectedValueOnce(new Error('offline'));
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.fileRecovery.discard' }));
    expect(await screen.findByRole('status')).toHaveTextContent('pages.fileRecovery.discardRefreshFailed');
    expect(api.discard).toHaveBeenCalledExactlyOnceWith({ recovery_id: 'backup', conversation_id: 'conversation', confirmed: true });
});
it('requires explicit confirmation before shortening existing backup retention', async () => {
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    const days = await screen.findByLabelText('pages.fileRecovery.days');
    fireEvent.change(days, { target: { value: '1' } });
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.save' }));
    const dialog = await screen.findByRole('alertdialog');
    expect(api.save).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole('button', { name: 'pages.fileRecovery.save' }));
    await waitFor(() => expect(api.save).toHaveBeenCalledWith({ retention_days: 1, max_bytes: 104857600 }));
});
it('rejects invalid retention and reports unfinished cleanup instead of claiming completion', async () => {
    api.cleanup.mockResolvedValue({ data: { pending_files: 1, unknown_outcomes: 0 } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    const days = await screen.findByLabelText('pages.fileRecovery.days');
    fireEvent.change(days, { target: { value: '0' } });
    expect(screen.getByRole('button', { name: 'pages.fileRecovery.save' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.retry' }));
    expect(await screen.findByRole('status')).toHaveTextContent('pages.fileRecovery.pending');
    expect(api.save).not.toHaveBeenCalled();
});

it('binds remote policy changes to the queried device authority and never calls local settings', async () => {
    api.remote.mockImplementation(async (body) => ({ data: { authority: 'authority', os_user: '501', outcome: body.request.command.operation === 'set_policy'
        ? { kind: 'policy', policy: { retention_days: 10, max_bytes: 104857600 } }
        : { kind: 'page', page: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0, records: [], next_cursor: null } } } }));
    render(<FileRecoverySettings target={{ connection: 'connection', device_id: 'public-device' }} />);
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    const days = await screen.findByLabelText('pages.fileRecovery.days');
    fireEvent.change(days, { target: { value: '10' } });
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.save' }));
    await waitFor(() => expect(api.remote).toHaveBeenCalledWith({ connection: 'connection', device_id: 'public-device',
        request: { expected_authority: 'authority', expected_os_user: '501', command: { operation: 'set_policy', retention_days: 10, max_bytes: 104857600 } } }));
    expect(api.query).not.toHaveBeenCalled();
    expect(api.save).not.toHaveBeenCalled();
});

it('pins a pending backup outside the cursor page and can discard it after confirmation', async () => {
    const record = { recovery_id: 'oldest', conversation_id: 'conversation', file_name: 'pending.txt', created_at_unix_ms: 1000,
        change_state: 'outcome_unknown', export_available: true };
    api.query.mockResolvedValue({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 100, reserved_bytes: 0,
        records: [], next_cursor: 'next', oldest_pending_record: record, oldest_pending_at_unix_ms: 1000 } });
    api.discard.mockResolvedValue({ data: { pending_files: 0, unknown_outcomes: 0 } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    expect(await screen.findByText('pending.txt')).toBeInTheDocument();
    expect(screen.queryByText('pages.fileRecovery.empty')).not.toBeInTheDocument();
    api.query.mockResolvedValueOnce({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 100, reserved_bytes: 0,
        records: [record], next_cursor: null, oldest_pending_record: record } });
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.more' }));
    await waitFor(() => expect(screen.queryByRole('button', { name: 'pages.fileRecovery.more' })).not.toBeInTheDocument());
    expect(screen.getAllByText('pending.txt')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.discard' }));
    fireEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'pages.fileRecovery.discard' }));
    await waitFor(() => expect(api.discard).toHaveBeenCalledExactlyOnceWith({ recovery_id: 'oldest', conversation_id: 'conversation', confirmed: true }));
});

it('shows a classified local storage failure without exposing raw diagnostic details', async () => {
    api.query.mockRejectedValueOnce(new RestResponseError('internal path and trace', 1, 'busy'));
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    expect(await screen.findByRole('status')).toHaveTextContent('pages.fileRecovery.error.busy');
    expect(screen.queryByText('internal path and trace')).not.toBeInTheDocument();
});

it('shows pending accounting separately from material cleanup and keeps export available', async () => {
    api.query.mockResolvedValue({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 100, reserved_bytes: 100,
        records: [], next_cursor: null, oldest_pending_record: { recovery_id: 'saved', conversation_id: 'conversation', file_name: 'saved.txt',
            created_at_unix_ms: 1000, expires_at_unix_ms: 2000, size_bytes: 10, material_state: 'saved', change_state: 'succeeded', cleanup_pending: false, cleanup_reason: 'quota_settlement_pending', export_available: true } } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    expect(await screen.findByText('pages.fileRecovery.error.quota_settlement_pending')).toBeInTheDocument();
    expect(screen.getByText('saved.txt')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'pages.fileRecovery.export' })).toBeEnabled();
    expect(screen.queryByText('pages.fileRecovery.pending')).not.toBeInTheDocument();
});

it('distinguishes a partial list from the final loaded records', async () => {
    api.query.mockResolvedValueOnce({ data: { policy: { retention_days: 7, max_bytes: 104857600 }, used_bytes: 0, reserved_bytes: 0, records: [], next_cursor: 'next' } });
    render(<FileRecoverySettings />);
    fireEvent.click(screen.getByText('pages.fileRecovery.manage'));
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.refresh' }));
    expect(await screen.findByText('pages.fileRecovery.loadedMore')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.more' }));
    expect(await screen.findByText('pages.fileRecovery.loaded')).toBeInTheDocument();
    expect(screen.queryByText('pages.fileRecovery.loadedMore')).not.toBeInTheDocument();
});
