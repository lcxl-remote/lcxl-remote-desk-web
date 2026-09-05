import { render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantContextNotices, noticeMessageId } from './assistant-context-notices';
import type { DeviceAssistantMessage } from './use-device-assistant-chat';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string) => key }) }));

describe('assistant context timeline notices', () => {
    it('keeps its original message boundary when later turns arrive', () => {
        const notice = { id: 'a', turnId: 'turn-a', kind: 'compacted' as const, afterMessageId: 'hidden-tool' };
        const messages: DeviceAssistantMessage[] = [{ id: 'user', role: 'user', text: 'question', contextBoundaryIds: ['user', 'hidden-tool'] }];
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
        expect(container.querySelector('details')?.open).toBe(false);
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain('unknownTime');
        expect(screen.getByTestId('assistant-context-notice').textContent).toContain('contextNotice.trimmed');
        rerender(<AssistantContextNotices notices={[]} />);
        expect(screen.queryByTestId('assistant-context-notice')).toBeNull();
    });
});
