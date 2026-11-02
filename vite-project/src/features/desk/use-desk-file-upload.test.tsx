import { useRef } from 'react';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useDeskFileUpload } from './use-desk-file-upload';
import { StubDataChannel } from '../file-manager/__tests__/harness';

vi.mock('react-i18next', () => {
    const t = (key: string, values?: Record<string, unknown>) => values?.name ? `${key}:${values.name}` : key;
    return { useTranslation: () => ({ t }) };
});
afterEach(cleanup);

function Desk({ channel, platform = 'Windows', enabled = true, release = () => {} }: {
    channel: StubDataChannel; platform?: string; enabled?: boolean; release?: () => void;
}) {
    const root = useRef<HTMLDivElement>(null);
    const upload = useDeskFileUpload({ channel: channel as unknown as RTCDataChannel, platform, enabled, root, releaseInputs: release });
    return <div ref={root} data-testid="desktop"><video /><div data-file-drop-ignore data-testid="attachment" />{upload.dropOverlay}<aside>{upload.uploadPanel}</aside></div>;
}
function data(name = 'empty.txt', folder = false) {
    const file = { name, size: 0, stream: () => new ReadableStream<Uint8Array>({ start(controller) { controller.close(); } }) } as File;
    return { types: ['Files'], dropEffect: 'copy', files: [file], items: [{ kind: 'file', getAsFile: () => file, webkitGetAsEntry: () => ({ isDirectory: folder }) }] };
}
function channel() { const value = new StubDataChannel(); value.readyState = 'open'; return value; }
const frames = (value: StubDataChannel) => value.textSent.map(frame => JSON.parse(frame));
const flush = async () => { await act(async () => { for (let i = 0; i < 16; i++) await Promise.resolve(); }); };

describe('desktop file drop', () => {
    it.each([['Android', true, 'unsupported'], ['Other', true, 'unsupported'], ['Windows', false, 'forbidden']])('rejects unsupported or forbidden targets before creating uploads', (platform, enabled, reason) => {
        const dc = channel();
        render(<Desk channel={dc} platform={platform as string} enabled={enabled as boolean} />);
        const transfer = data();
        fireEvent.dragEnter(screen.getByTestId('desktop'), { dataTransfer: transfer });
        fireEvent.dragOver(screen.getByTestId('desktop'), { dataTransfer: transfer });
        expect(transfer.dropEffect).toBe('none');
        expect(screen.getByText(`pages.desk.fileUpload.${reason}`)).toBeInTheDocument();
        fireEvent.drop(screen.getByTestId('desktop'), { dataTransfer: transfer });
        expect(dc.textSent).toHaveLength(0);
        expect(screen.queryByRole('region')).not.toBeInTheDocument();
    });

    it('keeps uploads serial, waits for confirmation, and isolates local controls', async () => {
        const dc = channel();
        const release = vi.fn();
        render(<Desk channel={dc} release={release} />);
        const desktop = screen.getByTestId('desktop');
        fireEvent.dragEnter(desktop, { dataTransfer: data() });
        expect(screen.getByText('pages.desk.fileUpload.drop')).toBeInTheDocument();
        fireEvent.drop(desktop, { dataTransfer: data('one.txt') });
        fireEvent.drop(desktop, { dataTransfer: data('two.txt') });
        expect(frames(dc).filter(frame => frame.type === 'upload_request')).toHaveLength(1);
        const id = frames(dc)[0].transfer_id;
        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'upload_response', transfer_id: id, accepted: true, resolved_target_dir: '/downloads', resolved_file_name: 'one (1).txt' }) }));
        await flush();
        expect(screen.getByText(/pages.desk.fileUpload.confirming/)).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'pages.desk.fileUpload.cancelFile:one.txt' })).not.toBeInTheDocument();
        fireEvent.click(screen.getByRole('button', { name: 'pages.desk.fileUpload.location' }));
        expect(screen.getByText('/downloads/one (1).txt')).toBeInTheDocument();
        expect(release).toHaveBeenCalled();
        act(() => dc.onmessage?.({ data: JSON.stringify({ type: 'transfer_complete', transfer_id: id, resolved_file_name: 'one (2).txt' }) }));
        await waitFor(() => expect(frames(dc).filter(frame => frame.type === 'upload_request')).toHaveLength(2));
        fireEvent.click(screen.getByRole('button', { name: 'pages.desk.fileUpload.cancelFile:two.txt' }));
        await flush();
        expect(frames(dc).filter(frame => frame.type === 'transfer_cancel')).toHaveLength(1);
        expect(screen.getByText('one (2).txt')).toBeInTheDocument();
    });

    it('rejects folders, ignores attachment targets, and only prevents file navigation', () => {
        const dc = channel(); render(<Desk channel={dc} />);
        const desktop = screen.getByTestId('desktop');
        fireEvent.dragEnter(screen.getByTestId('attachment'), { dataTransfer: data() });
        expect(screen.queryByText('pages.desk.fileUpload.drop')).not.toBeInTheDocument();
        fireEvent.drop(desktop, { dataTransfer: data('folder', true) });
        expect(screen.getByText('pages.desk.fileUpload.folder')).toBeInTheDocument();
        expect(dc.textSent).toHaveLength(0);
        expect(fireEvent.drop(document.body, { dataTransfer: data() })).toBe(false);
        expect(fireEvent.drop(document.body, { dataTransfer: { types: ['text/plain'] } })).toBe(true);
    });
});
