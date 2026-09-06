import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('assistant page header layout', () => {
    it('puts connection state in the conversation title without a separate status row', () => {
        const source = readFileSync('src/features/desk/device-assistant-page.tsx', 'utf8');
        expect(source).not.toContain('data-testid="assistant-signal-status"');
        expect(source.match(/<AssistantConnectionIcon /g)).toHaveLength(1);
        expect(source).toMatch(/<CardTitle[^>]*>\s*<AssistantConnectionIcon connected=\{isConnected\} enabled=\{assistantEnabled\}/);
        expect(source).toContain('<span title={chat.sessionTarget?.display_name}>');
    });
});
