import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantImages } from './assistant-images';
import { deleteAssistantImage, getAssistantImage, listAssistantImages } from '@/services/clients';
import type { DeviceAssistantVisualEvidence } from './device-assistant-event';
vi.mock('@/services/clients', () => ({ deleteAssistantImage: vi.fn(), getAssistantImage: vi.fn(), listAssistantImages: vi.fn() }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const frame: DeviceAssistantVisualEvidence = {
    content: { kind: 'artifact', artifact_id: 'visual-image', media_type: 'image/png', sha256: 'a'.repeat(64), size_bytes: 3 },
    schema_version: 1, evidence_id: 'visual-image', conversation_id: 'stored-run', focus_input_revision: 1,
    turn_id: 'turn', tool_call_id: 'capture-call', frame_id: 'frame', phase: 'observation', status: 'available',
    captured_at_unix_ms: 1000, expires_at_unix_ms: null, device_id: 'device', size_bytes: 3, media_type: 'image/png',
};
beforeEach(() => {
    vi.mocked(listAssistantImages).mockResolvedValue({ data: [frame], success: true, code: 0 });
    vi.mocked(getAssistantImage).mockResolvedValue(new Blob(['image'], { type: 'image/png' }) as never);
    vi.mocked(deleteAssistantImage).mockResolvedValue({ data: true, success: true, code: 0 });
    vi.stubGlobal('confirm', vi.fn(() => true));
    URL.createObjectURL = vi.fn(() => 'blob:stored-image');
    URL.revokeObjectURL = vi.fn();
});
afterEach(() => { cleanup(); vi.clearAllMocks(); vi.unstubAllGlobals(); });
describe('durable assistant images', () => {
    it('loads persisted images without any live preview and releases pixel URLs', async () => {
        const view = render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        const image = await screen.findByRole('img');
        expect(image.getAttribute('src')).toBe('blob:stored-image');
        expect(getAssistantImage).toHaveBeenCalledWith({ session: 'stored-run', attachment: 'visual-image' }, expect.objectContaining({ responseType: 'blob' }));
        view.unmount();
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:stored-image');
    });
    it('removes deleted attachments even when a stale snapshot still contains them', async () => {
        const view = render(<AssistantImages sessionId="stored-run" evidence={[frame]} />);
        await screen.findByRole('img');
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.imageDelete' }));
        await waitFor(() => expect(screen.queryByRole('img')).toBeNull());
        view.rerender(<AssistantImages sessionId="stored-run" evidence={[{ ...frame }]} />);
        await waitFor(() => expect(listAssistantImages).toHaveBeenCalledTimes(2));
        expect(screen.queryByRole('img')).toBeNull();
    });
    it('does not restore an owner-deleted image from a stale session after reopening', async () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[frame]} />);
        await waitFor(() => expect(listAssistantImages).toHaveBeenCalledTimes(1));
        expect(getAssistantImage).not.toHaveBeenCalled();
        expect(screen.queryByRole('img')).toBeNull();
    });
    it('does not treat an unavailable attachment as a retained live preview', async () => {
        vi.mocked(getAssistantImage).mockRejectedValue(new Error('not found'));
        render(<AssistantImages sessionId="stored-run" evidence={[{ ...frame, preview_data_url: 'data:image/png;base64,AQID' }]} />);
        await screen.findByText('pages.deviceAssistant.imageUnavailable');
        expect(screen.queryByRole('img')).toBeNull();
    });
});
