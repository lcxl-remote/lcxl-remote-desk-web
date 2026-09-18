import { describe, expect, it } from 'vitest';

import { isAiAssistantEnabled } from './ai-assistant-switch';

describe('isAiAssistantEnabled', () => {
    it('requires an explicit enabled projection', () => {
        expect(isAiAssistantEnabled({ ai_assistant_enabled: true })).toBe(true);
        expect(isAiAssistantEnabled({ ai_assistant_enabled: false })).toBe(false);
        expect(isAiAssistantEnabled({})).toBe(false);
        expect(isAiAssistantEnabled(null)).toBe(false);
    });
});
