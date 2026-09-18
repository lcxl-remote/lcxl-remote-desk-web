import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { AssistantAttachments, AssistantResultAttachments } from './assistant-attachments';
import { deleteAssistantAttachments, getAssistantAttachment, listAssistantAttachments, readAssistantAttachment } from '@/services/clients';

vi.mock('@/services/clients', () => ({
    deleteAssistantAttachments: vi.fn(), getAssistantAttachment: vi.fn(),
    listAssistantAttachments: vi.fn(), readAssistantAttachment: vi.fn(),
}));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const key = (name: string) => `pages.aiAssistant.attachments.${name}`;
const item = {
    attachment_id: 'attachment-a', message_id: 'message', tool_call_id: 'call', part: 'stdout',
    kind: 'text', media_type: 'text/plain', original_bytes: 5000, stored_bytes: 5000,
    source_truncated: false, storage_truncated: false, created_at_unix_ms: 1000, last_accessed_at_unix_ms: 1000,
    status: 'available', unavailable_at_unix_ms: null,
};
const listing = (status = 'available', kind = 'text') => ({ success: true, code: 0,
    data: { attachments: [{ ...item, status, kind }], used_bytes: status === 'available' ? 5000 : 0, capacity_bytes: 104857600, cursor: null },
});
beforeEach(() => {
    vi.mocked(listAssistantAttachments).mockResolvedValue(listing());
    vi.mocked(deleteAssistantAttachments).mockResolvedValue({ success: true, code: 0, data: true });
    vi.mocked(readAssistantAttachment).mockResolvedValue({ success: true, code: 0, data: {
        attachment_id: item.attachment_id, queries: [], body_bytes: 22, cursor: null,
        has_more: false, json_fragment: false, storage_truncated: false,
        lines: [{ line: 1, byte_offset_in_line: 0, text: '<script>bad()</script>', matched_queries: [], context: false, line_complete: true }],
    } });
    vi.stubGlobal('confirm', vi.fn(() => true));
    URL.createObjectURL = vi.fn(() => 'blob:attachment');
    URL.revokeObjectURL = vi.fn();
});
afterEach(() => { cleanup(); vi.resetAllMocks(); vi.unstubAllGlobals(); });
async function open() {
    fireEvent.click(screen.getByRole('button', { name: key('title') }));
    await screen.findByRole('button', { name: key('view') });
}

it('lists metadata only, including refresh and filters, until an explicit view', async () => {
    render(<AssistantAttachments sessionId="run" />);
    expect(listAssistantAttachments).not.toHaveBeenCalled();
    await open();
    fireEvent.click(screen.getByRole('button', { name: key('refresh') }));
    fireEvent.change(screen.getByRole('combobox', { name: key('type') }), { target: { value: 'text' } });
    expect(getAssistantAttachment).not.toHaveBeenCalled();
    expect(readAssistantAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    await screen.findByText('<script>bad()</script>');
    expect(document.querySelector('script')).toBeNull();
    expect(readAssistantAttachment).toHaveBeenCalledWith(expect.objectContaining({ session: 'run', attachment_id: item.attachment_id }), expect.anything());
});

it('searches multiple text keywords and offers no JSON content search', async () => {
    const view = render(<AssistantAttachments sessionId="run" />);
    await open();
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    await screen.findByText('<script>bad()</script>');
    fireEvent.change(screen.getByRole('textbox', { name: key('queries') }), { target: { value: 'error\n错误' } });
    fireEvent.click(screen.getByRole('button', { name: key('search') }));
    await waitFor(() => expect(readAssistantAttachment).toHaveBeenLastCalledWith(expect.objectContaining({ queries: ['error', '错误'] }), expect.anything()));
    vi.mocked(listAssistantAttachments).mockResolvedValue(listing('available', 'json'));
    view.rerender(<AssistantAttachments sessionId="json-run" />);
    await screen.findByRole('button', { name: key('view') });
    fireEvent.click(screen.getByRole('button', { name: key('view') }));
    expect(screen.queryByRole('textbox', { name: key('queries') })).toBeNull();
});

it('confirms deletion, refreshes quota and retains the tombstone without a new content read', async () => {
    render(<AssistantAttachments sessionId="run" />);
    await open();
    vi.mocked(listAssistantAttachments).mockResolvedValue(listing('deleted'));
    fireEvent.click(screen.getByRole('checkbox'));
    fireEvent.click(screen.getByRole('button', { name: /deleteSelected/ }));
    await waitFor(() => expect(deleteAssistantAttachments).toHaveBeenCalledWith({ session: 'run', attachment_ids: [item.attachment_id] }, expect.anything()));
    await waitFor(() => expect((screen.getByRole('button', { name: key('view') }) as HTMLButtonElement).disabled).toBe(true));
    expect(readAssistantAttachment).not.toHaveBeenCalled();
});

it('aborts old conversation requests and ignores a late metadata response', async () => {
    let finish: (value: ReturnType<typeof listing>) => void = () => {};
    vi.mocked(listAssistantAttachments).mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    const view = render(<AssistantAttachments sessionId="old-run" />);
    fireEvent.click(screen.getByRole('button', { name: key('title') }));
    await waitFor(() => expect(listAssistantAttachments).toHaveBeenCalledTimes(1));
    const oldSignal = vi.mocked(listAssistantAttachments).mock.calls[0][1]?.signal;
    view.rerender(<AssistantAttachments sessionId="new-run" />);
    await screen.findByRole('button', { name: key('view') });
    expect(oldSignal?.aborted).toBe(true);
    finish({ ...listing(), data: { ...listing().data, attachments: [{ ...item, attachment_id: 'old-only' }] } });
    await waitFor(() => expect(screen.queryByLabelText(`${key('select')} old-only`)).toBeNull());
});

it('opens only references from the result without fetching bodies automatically', async () => {
    vi.mocked(listAssistantAttachments).mockResolvedValue({ ...listing(), data: {
        ...listing().data, attachments: [item, { ...item, attachment_id: 'unrelated', part: 'other-stream' }],
    } });
    render(<AssistantResultAttachments sessionId="run" text={JSON.stringify({ parts: [{ content: { reference: { attachment_id: item.attachment_id } } }] })} />);
    expect(listAssistantAttachments).not.toHaveBeenCalled();
    await open();
    expect(screen.queryByText('other-stream')).toBeNull();
    expect(screen.getAllByRole('button', { name: key('view') })).toHaveLength(1);
    expect(readAssistantAttachment).not.toHaveBeenCalled();
    expect(getAssistantAttachment).not.toHaveBeenCalled();
});
