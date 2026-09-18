import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('assistant page header layout', () => {
    it('puts connection state in the conversation title without a separate status row', () => {
        const source = readFileSync('src/features/desk/ai-assistant-page.tsx', 'utf8');
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
    const page = readFileSync('src/features/desk/ai-assistant-page.tsx', 'utf8');
    const history = readFileSync('src/features/desk/assistant-history.tsx', 'utf8');
    expect(page).toContain('assistant-header shrink-0');
    for (const [source, icon, key] of [
        [history, 'History', 'pages.aiAssistant.history.title'],
        [page, 'CalendarClock', 'schedules.createResume'],
        [page, 'MessageSquarePlus', 'pages.aiAssistant.newConversation'],
    ]) {
        expect(source).toContain(`<${icon} className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('${key}')}</span>`);
        expect(source).toContain(`aria-label={t('${key}')}`);
    }
});

it('keeps standalone back navigation beside the conversation heading without the outer title or width cap', () => {
    const page = readFileSync('src/features/desk/ai-assistant-page.tsx', 'utf8');
    const standalone = page.slice(page.indexOf('export default function AiAssistantPage'));
    expect(standalone).not.toContain("t('pages.aiAssistant.title')");
    expect(standalone).not.toContain("t('pages.aiAssistant.subtitle')");
    expect(page).not.toContain('max-w-4xl');
    expect(standalone).toContain('absolute inset-0 flex min-w-0 flex-col gap-2 overflow-hidden');
    expect(standalone.match(/backTo=\{/g)).toHaveLength(2);
    const header = page.slice(page.indexOf('data-testid="assistant-title-row"'), page.indexOf('</CardHeader>', page.indexOf('data-testid="assistant-title-row"')));
    expect(header).toContain('{backTo && (');
    expect(header).toContain("aria-label={t('pages.aiAssistant.backToDevice')}");
    expect(header.indexOf('<Link to={backTo}')).toBeLessThan(header.indexOf('<CardTitle'));
});
