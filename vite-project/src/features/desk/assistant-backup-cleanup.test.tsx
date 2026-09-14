import { fireEvent, render, screen } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantBackupCleanup } from './assistant-backup-cleanup';
import { listFileRecoveryCleanup, retryFileRecoveryCleanup } from '@/services/clients';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock('@/services/clients', () => ({ listFileRecoveryCleanup: vi.fn(), retryFileRecoveryCleanup: vi.fn() }));
beforeEach(() => { vi.mocked(listFileRecoveryCleanup).mockReset(); vi.mocked(retryFileRecoveryCleanup).mockReset(); });
describe('deleted conversation cleanup', () => {
    it('queries independently of a live conversation and explains offline cleanup', async () => {
        vi.mocked(listFileRecoveryCleanup).mockResolvedValue({ data: { records: [{ conversation_id: 'deleted-id',
            created_at_unix_ms: 1000, next_attempt_at_unix_ms: 2000, attempts: 1, reason: 'offline' }], next_cursor: null } } as never);
        render(<AssistantBackupCleanup />);
        expect(listFileRecoveryCleanup).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.cleanupStatus.refresh' }));
        expect(await screen.findByText('pages.fileRecovery.cleanupStatus.offline')).toBeTruthy();
        expect(screen.queryByText('deleted-id')).toBeNull();
        vi.mocked(retryFileRecoveryCleanup).mockResolvedValue({ data: { scheduled: true } } as never);
        fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.retry' }));
        expect(await screen.findByText('pages.fileRecovery.cleanupStatus.scheduled')).toBeTruthy();
        expect(retryFileRecoveryCleanup).toHaveBeenCalledWith({ conversation_id: 'deleted-id' });
        expect(screen.queryByText('pages.fileRecovery.cleanupStatus.empty')).toBeNull();
    });
    it('does not claim cleanup completed when a status request fails', async () => {
        vi.mocked(listFileRecoveryCleanup).mockRejectedValue(new Error('Network unavailable'));
        render(<AssistantBackupCleanup />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.fileRecovery.cleanupStatus.refresh' }));
        expect(await screen.findByRole('alert')).toBeTruthy();
        expect(screen.queryByText('pages.fileRecovery.cleanupStatus.empty')).toBeNull();
    });
});
