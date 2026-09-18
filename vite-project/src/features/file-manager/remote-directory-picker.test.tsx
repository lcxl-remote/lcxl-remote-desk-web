import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { RemoteDirectoryPicker } from './remote-directory-picker';

const h = vi.hoisted(() => ({ list: vi.fn(), info: vi.fn(), close: vi.fn(), hook: vi.fn() }));
vi.mock('react-i18next', () => { const t = (key: string) => key; return { useTranslation: () => ({ t }) }; });
vi.mock('./use-file-transfer', () => ({ useFileTransfer: (...args: unknown[]) => {
    h.hook(...args);
    return { listFiles: h.list, querySystemInfo: h.info, closeConnection: h.close };
} }));
const key = (name: string) => `pages.aiAssistant.directories.${name}`;

describe('remote directory picker', () => {
    beforeEach(() => { vi.clearAllMocks(); h.info.mockResolvedValue({ name: 'Windows' }); });
    afterEach(cleanup);
    it.each(['target', null])('selects a drive for target %s with server-side filtering', async (target) => {
        h.list.mockResolvedValueOnce({ file_info_list: [{ name: 'C:\\', path: 'C:\\' }], total_count: 1 })
            .mockResolvedValue({ file_info_list: [], total_count: 0 });
        const select = vi.fn();
        const view = render(<RemoteDirectoryPicker deskId="device" sessionTargetId={target} disabled={false} onSelect={select} onCancel={() => {}} />);
        fireEvent.click(await screen.findByRole('button', { name: 'C:\\' }));
        await waitFor(() => expect(screen.getByRole('button', { name: key('select') })).toBeEnabled());
        fireEvent.click(screen.getByRole('button', { name: key('select') }));
        expect(select).toHaveBeenCalledWith('C:\\');
        expect(h.list).toHaveBeenLastCalledWith({ path: 'C:\\', page_no: 1, page_count: 100, directories_only: true });
        expect(h.hook).toHaveBeenCalledWith('device', undefined, target);
        view.unmount();
        expect(h.close).toHaveBeenCalled();
    });
    it('allows the Unix root, uses the server count for pagination, and ignores stale responses', async () => {
        h.info.mockResolvedValue({ name: 'Linux' });
        let resolve!: (value: unknown) => void;
        h.list.mockResolvedValueOnce({ file_info_list: [{ name: 'home', path: '/home' }], total_count: 101 })
            .mockImplementationOnce(() => new Promise(r => { resolve = r; }))
            .mockResolvedValue({ file_info_list: [], total_count: 0 });
        const select = vi.fn();
        const view = render(<RemoteDirectoryPicker deskId="device" sessionTargetId="target" disabled={false} onSelect={select} onCancel={() => {}} />);
        await screen.findByRole('button', { name: 'home' });
        fireEvent.click(screen.getByRole('button', { name: key('select') }));
        expect(select).toHaveBeenCalledWith('/');
        fireEvent.click(screen.getByRole('button', { name: key('next') }));
        await waitFor(() => expect(h.list).toHaveBeenLastCalledWith({ path: '/', page_no: 2, page_count: 100, directories_only: true }));
        view.unmount();
        await act(async () => resolve({ file_info_list: [{ name: 'stale', path: '/stale' }], total_count: 1 }));
        expect(screen.queryByText('stale')).toBeNull();
    });
    it('does not treat a refused listing as empty or allow selection before retry succeeds', async () => {
        h.info.mockResolvedValue({ name: 'Linux' });
        h.list.mockRejectedValueOnce(new Error('Access denied')).mockResolvedValue({ file_info_list: [], total_count: 0 });
        render(<RemoteDirectoryPicker deskId="device" sessionTargetId="target" disabled={false} onSelect={vi.fn()} onCancel={vi.fn()} />);
        expect(await screen.findByRole('alert')).toHaveTextContent('Access denied');
        expect(screen.getByRole('button', { name: key('select') })).toBeDisabled();
        fireEvent.click(screen.getByRole('button', { name: 'common.refresh' }));
        await waitFor(() => expect(screen.getByRole('button', { name: key('select') })).toBeEnabled());
    });
});
