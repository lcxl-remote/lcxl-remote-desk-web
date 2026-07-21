import { describe, expect, it } from 'vitest';

import {
    activeKinds,
    formatTransferBytes,
    type HostAccessSession,
} from '@/features/desk/host-access-status-page';

const session: HostAccessSession = {
    connection_id: 'connection-1',
    actor: { display_name: 'Alice', access_source: 'authenticated_account' },
    started_at: '2026-07-21T00:00:00Z',
    desktop_view: true,
    remote_control: false,
    terminal_count: 1,
    file_manager: true,
    transfers: [{
        transfer_id: 'transfer-1',
        direction: 'download',
        file_name: 'report.pdf',
        transferred_bytes: 512,
        total_bytes: 2048,
    }],
};

describe('host access status helpers', () => {
    it('summarizes every active capability in a session', () => {
        expect(activeKinds(session)).toEqual(['desktop', 'terminal', 'files', 'transfer']);
    });

    it('formats transfer sizes for compact status display', () => {
        expect(formatTransferBytes(512)).toBe('512 B');
        expect(formatTransferBytes(2048)).toBe('2.0 KiB');
        expect(formatTransferBytes(3 * 1024 * 1024)).toBe('3.0 MiB');
    });
});
