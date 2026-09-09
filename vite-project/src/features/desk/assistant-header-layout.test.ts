import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('assistant page header layout', () => {
    it('puts connection state in the conversation title without a separate status row', () => {
        const source = readFileSync('src/features/desk/device-assistant-page.tsx', 'utf8');
        expect(source).not.toContain('data-testid="assistant-signal-status"');
        expect(source.match(/<AssistantConnectionIcon /g)).toHaveLength(1);
        expect(source).toMatch(/<CardTitle[^>]*>\s*<AssistantConnectionIcon connected=\{isConnected\} enabled=\{assistantEnabled\}/);
        expect(source).toContain('<span title={chat.sessionTarget?.display_name} className="truncate">');
        expect(source).toContain('data-testid="assistant-title-row" className="flex items-center justify-between gap-2"');
        const header = source.slice(source.indexOf('<CardHeader className="assistant-header'), source.indexOf('</CardHeader>', source.indexOf('<CardHeader className="assistant-header')));
        expect(header.indexOf('<CardDescription>')).toBeGreaterThan(header.indexOf('onClick={resetConversation}'));
    });
});


it('gives all header actions icons, responsive labels and accessible names', () => {
    const page = readFileSync('src/features/desk/device-assistant-page.tsx', 'utf8');
    const history = readFileSync('src/features/desk/assistant-history.tsx', 'utf8');
    expect(page).toContain('assistant-header shrink-0');
    for (const [source, icon, key] of [
        [history, 'History', 'pages.deviceAssistant.history.title'],
        [page, 'CalendarClock', 'schedules.createResume'],
        [page, 'MessageSquarePlus', 'pages.deviceAssistant.newConversation'],
    ]) {
        expect(source).toContain(`<${icon} className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('${key}')}</span>`);
        expect(source).toContain(`aria-label={t('${key}')}`);
    }
});
