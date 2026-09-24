import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('assistant page header layout', () => {
    it('keeps device identity, connection state and conversation actions together', () => {
        const source = readFileSync('src/features/desk/ai-assistant-page.tsx', 'utf8');
        expect(source).not.toContain('data-testid="assistant-signal-status"');
        expect(source.match(/<AssistantConnectionIcon /g)).toHaveLength(1);
        expect(source).toMatch(/<CardTitle[^>]*>\s*<AssistantConnectionIcon connected=\{isConnected\} enabled=\{assistantEnabled\}/);
        expect(source).toContain('data-testid="assistant-title-row" className="flex items-center justify-between gap-2"');
        const header = source.slice(source.indexOf('<CardHeader className="assistant-header'), source.indexOf('</CardHeader>', source.indexOf('<CardHeader className="assistant-header')));
        expect(header).toContain('<AssistantHistory ');
        expect(header).toContain('<AssistantMoreMenu sections={moreSections} />');
        expect(header).not.toContain('<CardDescription>');
    });
});


it('keeps the context trigger below the input beside send or stop', () => {
    const page = readFileSync('src/features/desk/ai-assistant-page.tsx', 'utf8');
    const composer = page.slice(page.indexOf('<form onSubmit={submit} className="assistant-composer'), page.indexOf('</form>', page.indexOf('<form onSubmit={submit} className="assistant-composer')));
    const input = composer.indexOf('<Textarea');
    const actions = composer.indexOf('<div className="flex min-w-0 items-center gap-1">');
    const add = composer.indexOf("aria-label={t('pages.aiAssistant.workspace.addContext')}");
    const send = composer.indexOf('<Button type="submit" className="assistant-action"');
    expect(input).toBeGreaterThanOrEqual(0);
    expect(actions).toBeGreaterThan(input);
    expect(add).toBeGreaterThan(actions);
    expect(send).toBeGreaterThan(add);
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
