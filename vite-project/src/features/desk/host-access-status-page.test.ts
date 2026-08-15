import { describe, expect, it } from 'vitest';

import {
    activeKinds,
    formatTransferBytes,
    makePageBackgroundTransparent,
    type HostAccessSession,
} from '@/features/desk/host-access-status-page';

const session: HostAccessSession = {
    connection_id: 'connection-1',
    actor: { display_name: 'Alice', access_source: 'authenticated_account' },
    started_at: '2026-07-21T00:00:00Z',
    desktop_view: true,
    system_audio_capture: true,
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
        expect(activeKinds(session)).toEqual(['desktop', 'audio', 'terminal', 'files', 'transfer']);
    });

    it('formats transfer sizes for compact status display', () => {
        expect(formatTransferBytes(512)).toBe('512 B');
        expect(formatTransferBytes(2048)).toBe('2.0 KiB');
        expect(formatTransferBytes(3 * 1024 * 1024)).toBe('3.0 MiB');
    });

    it('makes the status window transparent and restores prior page styles', () => {
        const originalHtmlBackground = document.documentElement.style.background;
        const originalBodyBackground = document.body.style.background;
        document.documentElement.style.background = 'rgb(1, 2, 3)';
        document.body.style.background = 'rgb(4, 5, 6)';

        const restore = makePageBackgroundTransparent(document);

        expect(document.documentElement.style.getPropertyValue('background')).toBe('transparent');
        expect(document.body.style.getPropertyValue('background')).toBe('transparent');
        expect(document.body.style.getPropertyPriority('background')).toBe('important');

        restore();

        expect(document.documentElement.style.background).toBe('rgb(1, 2, 3)');
        expect(document.body.style.background).toBe('rgb(4, 5, 6)');

        document.documentElement.style.background = originalHtmlBackground;
        document.body.style.background = originalBodyBackground;
    });
});
