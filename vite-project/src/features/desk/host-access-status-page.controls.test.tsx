import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import HostAccessStatusPage, { type HostAccessSnapshot } from './host-access-status-page';

const { emit, uuidV4, REQUEST_ID } = vi.hoisted(() => {
    const requestId = '11111111-1111-4111-8111-111111111111';
    return {
        REQUEST_ID: requestId,
        emit: vi.fn(async () => undefined),
        uuidV4: vi.fn(() => requestId),
    };
});

vi.mock('@tauri-apps/api/event', () => ({ emit }));
// The page mints its correlation id with `uuid`'s `v4`, matching the rest of
// the desk feature. Stubbing `crypto.randomUUID` instead left the real id in
// place, so every assertion on the emitted payload compared against a random
// value and could never hold.
vi.mock('uuid', () => ({ v4: uuidV4 }));
vi.mock('react-i18next', () => ({
    useTranslation: () => ({ t: (key: string) => key }),
}));

const snapshot: HostAccessSnapshot = {
    epoch: 'epoch-1',
    revision: 1,
    indicator_enabled: true,
    total_session_count: 1,
    sessions: [{
        connection_id: 'connection-1',
        actor: { display_name: 'Alice', access_source: 'authenticated_account' },
        started_at: '2026-07-22T00:00:00Z',
        desktop_view: true,
        system_audio_capture: true,
        remote_control: true,
        terminal_count: 0,
        file_manager: false,
        transfers: [],
    }],
    remote_access: {
        mode: 'unlocked',
        state_version: 3,
        locked_at: null,
        durable: true,
        central_sync: 'not_required',
    },
};

describe('host access status controls', () => {
    beforeEach(() => {
        emit.mockClear();
        Object.defineProperty(window, '__lcxlHostAccessSnapshot', {
            configurable: true,
            value: snapshot,
        });
        uuidV4.mockReturnValue(REQUEST_ID);
    });

    afterEach(() => vi.restoreAllMocks());

    it('shows active system audio capture in the collapsed summary', () => {
        render(<HostAccessStatusPage />);

        expect(screen.getByText('hostAccess.systemAudio')).toBeInTheDocument();
    });

    it('waits for the daemon result instead of optimistically changing lock state', async () => {
        render(<HostAccessStatusPage />);

        fireEvent.click(screen.getByRole('button', { name: 'hostAccess.lockAll' }));
        await waitFor(() => expect(emit).toHaveBeenCalledWith(
            'lcxl-host-access-control',
            {
                request_id: REQUEST_ID,
                action: 'lock',
            },
        ));
        expect(screen.getByRole('button', { name: 'hostAccess.applying' })).toBeDisabled();
        expect(screen.queryByText('hostAccess.lockedTitle')).not.toBeInTheDocument();

        act(() => window.dispatchEvent(new CustomEvent('lcxl-host-access-control-result', {
            detail: {
                request_id: REQUEST_ID,
                ok: true,
                error: null,
            },
        })));
        await waitFor(() => expect(
            screen.getByRole('button', { name: 'hostAccess.lockAll' }),
        ).toBeEnabled());
        expect(screen.queryByText('hostAccess.lockedTitle')).not.toBeInTheDocument();
    });

    it('marks only the target session as disconnecting until the daemon result', async () => {
        render(<HostAccessStatusPage />);
        fireEvent.click(screen.getByRole('button', { name: 'hostAccess.toggleDetails' }));
        const disconnect = await screen.findByRole('button', { name: 'hostAccess.disconnect' });

        fireEvent.click(disconnect);
        fireEvent.click(disconnect);

        await waitFor(() => expect(emit).toHaveBeenCalledTimes(1));
        expect(screen.getByRole('button', { name: 'hostAccess.disconnecting' })).toBeDisabled();
        act(() => window.dispatchEvent(new CustomEvent('lcxl-host-access-control-result', {
            detail: {
                request_id: REQUEST_ID,
                ok: true,
                error: null,
            },
        })));
        await waitFor(() => expect(
            screen.getByRole('button', { name: 'hostAccess.disconnect' }),
        ).toBeEnabled());
    });

    it('allows local unlock while central synchronization is pending', async () => {
        Object.defineProperty(window, '__lcxlHostAccessSnapshot', {
            configurable: true,
            value: {
                ...snapshot,
                total_session_count: 0,
                sessions: [],
                remote_access: {
                    mode: 'locked',
                    state_version: 4,
                    locked_at: '2026-07-22T00:00:00Z',
                    durable: true,
                    central_sync: 'pending',
                },
            } satisfies HostAccessSnapshot,
        });

        render(<HostAccessStatusPage />);

        expect(screen.getByText('hostAccess.lockedTitle')).toBeInTheDocument();
        expect(screen.getByText('hostAccess.centralSyncPending')).toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'hostAccess.unlock' })).toBeEnabled();
        fireEvent.click(screen.getByRole('button', { name: 'hostAccess.unlock' }));
        await waitFor(() => expect(emit).toHaveBeenCalledWith(
            'lcxl-host-access-control',
            {
                request_id: REQUEST_ID,
                action: 'unlock',
                expected_version: 4,
            },
        ));
    });

    it('allows an authenticated local unlock from recovery-locked state', () => {
        Object.defineProperty(window, '__lcxlHostAccessSnapshot', {
            configurable: true,
            value: {
                ...snapshot,
                total_session_count: 0,
                sessions: [],
                remote_access: {
                    mode: 'recovery_locked',
                    state_version: 0,
                    locked_at: null,
                    durable: false,
                    central_sync: 'pending',
                },
            } satisfies HostAccessSnapshot,
        });

        render(<HostAccessStatusPage />);

        expect(screen.getByRole('button', { name: 'hostAccess.unlock' })).toBeEnabled();
        expect(screen.queryByRole('button', { name: 'hostAccess.retryLock' })).not.toBeInTheDocument();
    });

    it('offers a lock retry instead of unlock when persistence failed', async () => {
        Object.defineProperty(window, '__lcxlHostAccessSnapshot', {
            configurable: true,
            value: {
                ...snapshot,
                total_session_count: 0,
                sessions: [],
                remote_access: {
                    mode: 'locked',
                    state_version: 4,
                    locked_at: '2026-07-22T00:00:00Z',
                    durable: false,
                    central_sync: 'pending',
                },
            } satisfies HostAccessSnapshot,
        });
        render(<HostAccessStatusPage />);

        fireEvent.click(screen.getByRole('button', { name: 'hostAccess.retryLock' }));
        await waitFor(() => expect(emit).toHaveBeenCalledWith(
            'lcxl-host-access-control',
            {
                request_id: REQUEST_ID,
                action: 'lock',
            },
        ));
        expect(screen.queryByRole('button', { name: 'hostAccess.unlock' })).not.toBeInTheDocument();
    });
});
