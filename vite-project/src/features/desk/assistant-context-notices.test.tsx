import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';
import { AssistantTranscript } from './assistant-transcript';
import type { AiAssistantMessage } from './use-ai-assistant-chat';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' },
    t: (key: string, values?: Record<string, unknown>) => values?.index ? `${key}:${values.index}` : key }) }));
const summary = vi.hoisted(() => vi.fn());
vi.mock('@/services/clients', async original => ({
    ...(await original<typeof import('@/services/clients')>()), getAiAssistantContextSummary: summary }));

describe('assistant context timeline notices', () => {
    it.each(['refreshed', 'restricted', 'automation_limit', 'resume_stopped'] as const)('distinguishes %s from capacity trimming', kind => {
        const { unmount } = render(<AssistantContextNotices notices={[{ id: kind, turnId: 'turn', kind }]} />);
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain(`contextNotice.${kind}`);
        expect(screen.getByTestId('assistant-context-notice').textContent).not.toContain('contextNotice.trimmed');
        unmount();
    });
    it('keeps its original message boundary when later turns arrive', () => {
        const notice = { id: 'a', turnId: 'turn-a', kind: 'compacted' as const, afterMessageId: 'hidden-tool' };
        const messages: AiAssistantMessage[] = [{ id: 'user', role: 'user', text: 'question', contextBoundaryIds: ['user', 'hidden-tool'] }];
        expect(noticeMessageId(notice, messages)).toBe('user');
        messages.push({ id: 'later', role: 'assistant', text: 'later answer' });
        expect(noticeMessageId(notice, messages)).toBe('user');
        expect(noticeMessageId(notice, messages.slice(1))).toBeUndefined();
    });

    it('shows recorded time without inventing an occurrence time for old history', () => {
        const notice = { id: 'a', turnId: 'turn-a', kind: 'compacted' as const, createdAt: '2026-09-05T15:00:00Z' };
        const { container, rerender } = render(<AssistantContextNotices notices={[notice]} />);
        expect(container.querySelector('time')?.dateTime).toBe(notice.createdAt);
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain('contextNotice.compacted');
        rerender(<AssistantContextNotices historical notices={[{ id: 'b', turnId: 'old', kind: 'trimmed' }]} />);
        expect(container.querySelector('time')).toBeNull();
        expect(container.querySelector('[data-slot="disclosure"]')?.getAttribute('data-state') === 'open').toBe(false);
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain('unknownTime');
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain('contextNotice.trimmed');
        rerender(<AssistantContextNotices notices={[]} />);
        expect(screen.queryByTestId('assistant-context-notice')).toBeNull();
    });

    describe('compaction summary', () => {
        const notice = { id: 'c', turnId: 'turn-7', kind: 'compacted' as const, checkpointGeneration: 1, afterMessageId: 'old-a' };
        const available = { success: true, code: 0, data: { state: 'available', summary: { createdAt: 'now', sections: [
            { kind: 'goals', facts: [{ text: 'Keep <b>the</b> goal', sourceMessageIds: ['old-a', 'old-b'] }] },
            { kind: 'next_steps', facts: [{ text: 'Run the report', sourceMessageIds: [] }] },
        ] } } };
        beforeEach(() => summary.mockReset());

        it('loads only when opened and shows sections as plain text', async () => {
            summary.mockResolvedValue(available);
            render(<AssistantContextNotices sessionId="session-1" notices={[notice]} />);
            expect(summary).not.toHaveBeenCalled();
            fireEvent.click(screen.getByText('pages.aiAssistant.contextSummary.show'));
            await screen.findByText('pages.aiAssistant.contextSummary.kind.goals');
            expect(summary).toHaveBeenCalledWith({ session: 'session-1', turn: 'turn-7', generation: 1 });
            expect(screen.getByText('Keep <b>the</b> goal')).toBeInTheDocument();
            expect(screen.getByText('pages.aiAssistant.contextSummary.kind.next_steps')).toBeInTheDocument();
            // Without navigation no source jump is offered.
            expect(screen.queryByText(/contextSummary.source/)).toBeNull();
        });

        it.each(['superseded', 'unavailable'] as const)('explains a %s summary', async state => {
            summary.mockResolvedValue({ success: true, code: 0, data: { state, summary: null } });
            render(<AssistantContextNotices sessionId="session-1" notices={[notice]} />);
            fireEvent.click(screen.getByText('pages.aiAssistant.contextSummary.show'));
            expect(await screen.findByText(`pages.aiAssistant.contextSummary.${state}`)).toBeInTheDocument();
        });

        it('offers a retry after a failed load', async () => {
            summary.mockResolvedValueOnce({ success: false, code: 1 }).mockResolvedValueOnce(available);
            render(<AssistantContextNotices sessionId="session-1" notices={[notice]} />);
            fireEvent.click(screen.getByText('pages.aiAssistant.contextSummary.show'));
            fireEvent.click(await screen.findByText('pages.aiAssistant.contextSummary.retry'));
            await screen.findByText('Keep <b>the</b> goal');
            expect(summary).toHaveBeenCalledTimes(2);
        });

        it('is not offered without a session or a generation', () => {
            const { rerender } = render(<AssistantContextNotices notices={[notice]} />);
            expect(screen.queryByText('pages.aiAssistant.contextSummary.show')).toBeNull();
            rerender(<AssistantContextNotices sessionId="s" notices={[{ ...notice, checkpointGeneration: undefined }]} />);
            expect(screen.queryByText('pages.aiAssistant.contextSummary.show')).toBeNull();
        });

        it('jumps to loaded sources and asks the page to load others', async () => {
            summary.mockResolvedValue(available);
            const messages: AiAssistantMessage[] = [{ id: 'old-a', role: 'user', text: 'original question' }];
            const reveal = vi.fn(async () => true);
            const { rerender } = render(<AssistantTranscript sessionId="session-1" messages={messages} tools={[]} running={false} notices={[notice]} />);
            fireEvent.click(screen.getByText('pages.aiAssistant.contextSummary.show'));
            // Only the loaded source can be reached without a page loader.
            fireEvent.click(await screen.findByText('pages.aiAssistant.contextSummary.source'));
            await waitFor(() => expect(document.querySelector('[data-source-anchor]')?.getAttribute('data-assistant-message-id')).toBe('old-a'));
            rerender(<AssistantTranscript sessionId="session-1" messages={messages} tools={[]} running={false} notices={[notice]} revealSource={reveal} />);
            fireEvent.click(screen.getByText('pages.aiAssistant.contextSummary.sourceNumbered:2'));
            await waitFor(() => expect(reveal).toHaveBeenCalledWith('old-b'));
        });
    });
});
