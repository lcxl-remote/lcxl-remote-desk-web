import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantImages } from './assistant-images';
import { AssistantReasoning } from './assistant-reasoning';
import { AssistantToolCall } from './assistant-tool-call';
import { AssistantToolGroup } from './assistant-tool-group';
import { deleteAssistantImage, getAssistantImage, listAssistantImages } from '@/services/clients';
import type { AiAssistantVisualEvidence } from './ai-assistant-event';
vi.mock('@/services/clients', () => ({ deleteAssistantImage: vi.fn(), getAssistantImage: vi.fn(), listAssistantImages: vi.fn() }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string, values?: { count?: number; time?: string }) =>
    values?.time !== undefined ? `${key}:${values.time}` : values?.count === undefined ? key : `${key}:${values.count}` }) }));
const frame: AiAssistantVisualEvidence = {
    content: { kind: 'artifact', artifact_id: 'visual-image', media_type: 'image/png', sha256: 'a'.repeat(64), size_bytes: 3 },
    schema_version: 1, evidence_id: 'visual-image', conversation_id: 'stored-run', focus_input_revision: 1,
    turn_id: 'turn', tool_call_id: 'capture-call', frame_id: 'frame', phase: 'observation', status: 'available',
    captured_at_unix_ms: 1000, expires_at_unix_ms: null, device_id: 'device', size_bytes: 3, media_type: 'image/png',
};
beforeEach(() => {
    vi.mocked(listAssistantImages).mockResolvedValue({ data: [frame], success: true, code: 0 });
    vi.mocked(getAssistantImage).mockResolvedValue(new Blob(['image'], { type: 'image/png' }) as never);
    vi.mocked(deleteAssistantImage).mockResolvedValue({ data: true, success: true, code: 0 });
    URL.createObjectURL = vi.fn(() => 'blob:stored-image');
    URL.revokeObjectURL = vi.fn();
});
afterEach(() => { cleanup(); vi.clearAllMocks(); vi.unstubAllGlobals(); });
describe('durable assistant images', () => {
    it('labels the stored observation record time and preserves it when reopening', async () => {
        const recordedAt = Date.UTC(2026, 9, 10, 12, 30);
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [{ ...frame, captured_at_unix_ms: recordedAt }], success: true, code: 0 });
        const view = render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        const text = `pages.aiAssistant.imageRecordedAt:${new Date(recordedAt).toLocaleString()}`;
        expect(await screen.findByText(text)).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.imageTimeUnrecorded')).toBeNull();
        view.unmount();
        render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        expect(await screen.findByText(text)).toBeTruthy();
    });

    it.each([0, -1, Number.NaN, Number.POSITIVE_INFINITY, Number.MAX_SAFE_INTEGER + 1, 8_640_000_000_000_001])(
        'shows time not recorded for invalid timestamp %s instead of an epoch or invalid date', async capturedAt => {
            vi.mocked(listAssistantImages).mockResolvedValue({ data: [{ ...frame, captured_at_unix_ms: capturedAt }], success: true, code: 0 });
            render(<AssistantImages sessionId="stored-run" evidence={[]} />);
            expect(await screen.findByText('pages.aiAssistant.imageTimeUnrecorded')).toBeTruthy();
            expect(screen.queryByText(/pages.aiAssistant.imageRecordedAt/)).toBeNull();
            expect(screen.queryByText(/1970|1969|Invalid Date/)).toBeNull();
        },
    );

    it('does not load durable pixels while listing conversation history', async () => {
        render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' });
        expect(getAssistantImage).not.toHaveBeenCalled();
        expect(screen.queryByRole('img')).toBeNull();
    });
    it('shows saved observation timing without loading pixels or promoting the frame to fresh', async () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [{ ...frame, frame_observation: {
            freshness: 'latest_observed', received_at_unix_ms: 1000, receipt_age_ms: 2500,
        } }], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        expect(await screen.findByText('pages.aiAssistant.observation.latestObserved')).toBeVisible();
        expect(screen.getByText('pages.aiAssistant.observation.historicalFrame')).toBeVisible();
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
        expect(getAssistantImage).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        await screen.findByRole('img');
        expect(screen.getByText('pages.aiAssistant.observation.latestObserved')).toBeVisible();
        expect(screen.queryByText('pages.aiAssistant.observation.freshFrame')).toBeNull();
    });
    it('inserts each screenshot after its tool record and before the next reply', async () => {
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'question', role: 'user', text: 'Capture the window' },
            { id: 'capture', role: 'tool_call', toolCallId: 'capture-call', text: 'Screenshot tool' },
            { id: 'answer', role: 'assistant', text: 'The result is visible' },
        ]} renderMessage={message => <p>{message.text}</p>} />);
        const image = await screen.findByRole('img');
        expect(screen.getByText('Screenshot tool').compareDocumentPosition(image) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        expect(image.compareDocumentPosition(screen.getByText('The result is visible')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        expect(screen.queryByText('pages.aiAssistant.imageEarlier')).toBeNull();
    });
    it('moves an older screenshot to its tool when earlier messages are loaded without duplicating it', async () => {
        const renderMessage = (message: { text: string }) => <p>{message.text}</p>;
        const view = render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'answer', role: 'assistant', text: 'Later reply' },
        ]} renderMessage={renderMessage} />);
        fireEvent.click(await screen.findByRole('button', { name: /pages.aiAssistant.imageEarlier/ }));
        fireEvent.click(await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        await screen.findByRole('img');
        expect(screen.getByText('pages.aiAssistant.imageEarlier')).toBeTruthy();
        view.rerender(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'capture', role: 'tool_call', toolCallId: 'capture-call', text: 'Earlier capture' },
            { id: 'answer', role: 'assistant', text: 'Later reply' },
        ]} renderMessage={renderMessage} />);
        await waitFor(() => expect(screen.queryByText('pages.aiAssistant.imageEarlier')).toBeNull());
        const button = await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' });
        expect(screen.getAllByRole('button', { name: 'pages.aiAssistant.imageOpen' })).toHaveLength(1);
        expect(screen.getByText('Earlier capture').compareDocumentPosition(button) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        expect(getAssistantImage).toHaveBeenCalledTimes(2);
    });
    it('uses the server turn when a screenshot has no tool call and splits adjacent turns', async () => {
        const turnFrame = { ...frame, tool_call_id: '' };
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [turnFrame], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'tool_call', toolCallId: 'other-call', turnId: 'earlier-turn', text: 'First' },
            { id: 'second', role: 'tool_call', toolCallId: 'another-call', turnId: 'turn', text: 'Second' },
            { id: 'answer', role: 'assistant', turnId: 'turn', text: 'Answer' },
        ]} renderMessage={message => <p>{message.text}</p>}
            renderToolGroup={group => <p>{group.map(message => message.text).join(', ')}</p>} />);
        expect(await screen.findByText('First')).toBeTruthy();
        expect(screen.getByText('Second')).toBeTruthy();
        expect(screen.queryByText('First, Second')).toBeNull();
        expect(screen.queryByText('pages.aiAssistant.imageEarlier')).toBeNull();
        const imageButton = await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' });
        expect(screen.getByText('Second').compareDocumentPosition(imageButton) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        expect(imageButton.compareDocumentPosition(screen.getByText('Answer')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });
    it('keeps a screenshot with an unloaded explicit tool call in materials', async () => {
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'second', role: 'tool_call', toolCallId: 'another-call', turnId: 'turn', text: 'Second' },
        ]} renderMessage={message => <p>{message.text}</p>} />);
        expect(await screen.findByRole('button', { name: /pages.aiAssistant.imageEarlier/ })).toBeTruthy();
        expect(screen.queryByRole('button', { name: 'pages.aiAssistant.imageOpen' })).toBeNull();
    });
    it('loads persisted images without any live preview and releases pixel URLs', async () => {
        const view = render(<AssistantImages sessionId="stored-run" evidence={[]} />);
        fireEvent.click(await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        const image = await screen.findByRole('img');
        expect(image.getAttribute('src')).toBe('blob:stored-image');
        expect(getAssistantImage).toHaveBeenCalledWith({ session: 'stored-run', attachment: 'visual-image' }, expect.objectContaining({ responseType: 'blob' }));
        view.unmount();
        expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:stored-image');
    });
    it('removes deleted attachments even when a stale snapshot still contains them', async () => {
        const view = render(<AssistantImages sessionId="stored-run" evidence={[frame]} />);
        fireEvent.click(await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        await screen.findByRole('img');
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.imageDelete' }));
        expect(deleteAssistantImage).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button', { name: 'common.confirm' }));
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
        fireEvent.click(await screen.findByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        await screen.findByText('pages.aiAssistant.imageUnavailable');
        expect(screen.queryByRole('img')).toBeNull();
    });
});

describe('assistant activity folding', () => {
    const renderMessage = (message: { text: string }) => <p>{message.text}</p>;
    const renderReasoning = (message: { reasoning?: string | null }) => <details><summary>Thinking</summary>{message.reasoning}</details>;
    const renderToolGroup = (group: { text: string }[]) => <details><summary>Tools</summary>{group.map(item => <p key={item.text}>{item.text}</p>)}</details>;

    it('folds multiple activities between assistant bodies and keeps nested disclosures independent', async () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'assistant', text: 'First answer' },
            { id: 'capture', role: 'tool_call', turnId: 'turn', toolCallId: 'capture-call', text: 'Capture tool' },
            { id: 'result', role: 'tool_result', turnId: 'turn', toolCallId: 'capture-call', text: 'Capture result' },
            { id: 'second', role: 'assistant', text: 'Second answer', reasoning: 'Checked the image' },
        ]} renderMessage={renderMessage} renderReasoning={renderReasoning} renderToolGroup={renderToolGroup} />);
        const outer = screen.getByRole('button', { name: 'pages.aiAssistant.activityGroup:2' });
        expect(outer.getAttribute('data-state')).toBe('closed');
        expect(screen.queryByRole('button', { name: 'pages.aiAssistant.imageOpen' })).toBeNull();
        fireEvent.click(outer);
        expect(outer.getAttribute('data-state')).toBe('open');
        expect(screen.getByText('Tools').closest('details')?.open).toBe(false);
        expect(screen.getByText('Thinking').closest('details')?.open).toBe(false);
        expect(screen.getByText('Capture tool').compareDocumentPosition(screen.getByText('Checked the image')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
        expect(screen.getByText('Checked the image').compareDocumentPosition(screen.getByText('Second answer')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    });

    it('expands a screenshot process and displays its thumbnail while keeping sibling tools and reasoning folded', async () => {
        const tools = [
            { callId: 'read-call', name: 'inspect_desktop_ui', status: 'ok' as const, argumentsJson: '{}', output: 'UI result' },
            { callId: 'capture-call', name: 'read_current_screen', status: 'ok' as const, argumentsJson: '{}', output: 'Capture result' },
        ];
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'assistant', text: 'First answer' },
            { id: 'read', role: 'tool_call', turnId: 'read-turn', toolCallId: 'read-call', text: '' },
            { id: 'capture', role: 'tool_call', turnId: 'turn', toolCallId: 'capture-call', text: '' },
            { id: 'second', role: 'assistant', text: 'Second answer', reasoning: 'Checked the image' },
        ]} renderMessage={message => message.role === 'tool_call'
            ? <AssistantToolCall tool={tools.find(tool => tool.callId === message.toolCallId)} running={false} /> : renderMessage(message)}
            renderReasoning={message => <AssistantReasoning text={message.reasoning} />}
            renderToolGroup={group => <AssistantToolGroup messages={group} tools={tools}
                renderMessage={message => <AssistantToolCall tool={tools.find(tool => tool.callId === message.toolCallId)} running={false} />} />} />);
        const image = await screen.findByRole('img', { name: 'pages.aiAssistant.visualEvidenceAlt' });
        expect(image.closest('[data-state="closed"]')).toBeNull();
        const outer = screen.getByRole('button', { name: 'pages.aiAssistant.activityGroup:3' });
        expect(outer.getAttribute('data-state')).toBe('open');
        expect(screen.getAllByRole('button', { name: /pages.aiAssistant.workspace.toolGroup:1/ })).toHaveLength(2);
        for (const button of screen.getAllByRole('button', { name: /pages.aiAssistant.workspace.toolGroup:1/ })) {
            expect(button.getAttribute('data-state')).toBe('closed');
        }
        expect(screen.getByRole('button', { name: 'pages.aiAssistant.reasoning' }).getAttribute('data-state')).toBe('closed');
        expect(screen.getByText('Checked the image').closest('[data-state="closed"]')).toBeTruthy();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.imageOpen' }));
        expect(await screen.findByRole('dialog')).toBeVisible();
    });

    it('opens when screenshots arrive asynchronously and preserves a subsequent manual collapse', async () => {
        let resolveImages!: (value: Awaited<ReturnType<typeof listAssistantImages>>) => void;
        vi.mocked(listAssistantImages).mockReturnValue(new Promise(resolve => { resolveImages = resolve; }));
        const messages = [
            { id: 'first', role: 'assistant' as const, text: 'First answer' },
            { id: 'capture', role: 'tool_call' as const, turnId: 'turn', toolCallId: 'capture-call', text: 'Capture tool' },
            { id: 'second', role: 'assistant' as const, text: 'Second answer', reasoning: 'Checked the image' },
        ];
        const evidence: AiAssistantVisualEvidence[] = [];
        const view = render(<AssistantImages sessionId="stored-run" evidence={evidence} messages={messages}
            renderMessage={renderMessage} renderReasoning={renderReasoning} renderToolGroup={renderToolGroup} />);
        const outer = screen.getByRole('button', { name: 'pages.aiAssistant.activityGroup:2' });
        expect(outer.getAttribute('data-state')).toBe('closed');
        resolveImages({ data: [frame], success: true, code: 0 });
        expect(await screen.findByRole('img')).toBeVisible();
        expect(outer.getAttribute('data-state')).toBe('open');
        fireEvent.click(outer);
        expect(outer.getAttribute('data-state')).toBe('closed');
        view.rerender(<AssistantImages sessionId="stored-run" evidence={evidence}
            messages={messages.map(message => message.id === 'second' ? { ...message, text: 'Updated answer' } : message)}
            renderMessage={renderMessage} renderReasoning={renderReasoning} renderToolGroup={renderToolGroup} />);
        expect(outer.getAttribute('data-state')).toBe('closed');
        expect(screen.getByAltText('pages.aiAssistant.visualEvidenceAlt').closest('[data-state="closed"]')).toBeTruthy();
        expect(getAssistantImage).toHaveBeenCalledTimes(1);
    });

    it('keeps a single activity visible and folds two calls even when they share one tool group', () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [], success: true, code: 0 });
        const view = render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'assistant', text: 'First' },
            { id: 'only-call', role: 'tool_call', turnId: 'one', toolCallId: 'one', text: 'One call' },
            { id: 'second', role: 'assistant', text: 'Second' },
        ]} renderMessage={renderMessage} renderToolGroup={renderToolGroup} />);
        expect(screen.queryByRole('button', { name: /pages.aiAssistant.activityGroup/ })).toBeNull();
        view.rerender(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'assistant', text: 'First' },
            { id: 'call-one', role: 'tool_call', turnId: 'one', toolCallId: 'one', text: 'One call' },
            { id: 'call-two', role: 'tool_call', turnId: 'one', toolCallId: 'two', text: 'Two calls' },
            { id: 'second', role: 'assistant', text: 'Second' },
        ]} renderMessage={renderMessage} renderToolGroup={renderToolGroup} />);
        expect(screen.getAllByRole('button', { name: 'pages.aiAssistant.activityGroup:2' })).toHaveLength(1);
    });

    it('does not fold across a user message or hide unfinished activity', () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'early', role: 'tool_call', turnId: 'start', toolCallId: 'early', text: 'Early call' },
            { id: 'first', role: 'assistant', text: 'First' },
            { id: 'call-one', role: 'tool_call', turnId: 'one', toolCallId: 'one', text: 'First call' },
            { id: 'question', role: 'user', text: 'New question' },
            { id: 'call-two', role: 'tool_call', turnId: 'two', toolCallId: 'two', text: 'Second call' },
            { id: 'second', role: 'assistant', text: 'Second' },
            { id: 'call-three', role: 'tool_call', turnId: 'three', toolCallId: 'three', text: 'Unfinished call' },
            { id: 'thought', role: 'assistant', text: '', reasoning: 'Still thinking' },
        ]} renderMessage={renderMessage} renderReasoning={renderReasoning} renderToolGroup={renderToolGroup} />);
        expect(screen.queryByRole('button', { name: /pages.aiAssistant.activityGroup/ })).toBeNull();
        expect(screen.getByText('Early call')).toBeTruthy();
        expect(screen.getByText('Unfinished call')).toBeTruthy();
        expect(screen.getByText('Still thinking')).toBeTruthy();
    });
    it('keeps a committed goal-review reason outside folded tool activity', () => {
        vi.mocked(listAssistantImages).mockResolvedValue({ data: [], success: true, code: 0 });
        render(<AssistantImages sessionId="stored-run" evidence={[]} messages={[
            { id: 'first', role: 'assistant', text: 'Proposing a goal' },
            { id: 'one', role: 'tool_call', toolCallId: 'one', text: 'One' },
            { id: 'two', role: 'tool_call', toolCallId: 'two', text: 'Two' },
            { id: 'decision', role: 'goal_decision', text: 'The goal matches your request', goalDecision: { status: 'approved', reviewer: 'approval_ai' } },
            { id: 'next', role: 'assistant', text: 'Starting the approved goal' },
        ]} renderMessage={renderMessage} renderToolGroup={renderToolGroup} />);
        expect(screen.getByText('The goal matches your request').closest('[data-state="closed"]')).toBeNull();
    });

});
