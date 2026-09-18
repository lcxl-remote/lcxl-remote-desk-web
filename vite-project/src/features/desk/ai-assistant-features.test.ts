import { describe, expect, it } from 'vitest';

import {
    OSS_AI_ASSISTANT_FEATURES,
    hasAiAssistantBrowserEntry,
} from './ai-assistant-features';

describe('AI Assistant feature profile', () => {
    it('keeps the Manager browser entry while independently gating unfinished controls', () => {
        const manager = {
            ...OSS_AI_ASSISTANT_FEATURES,
            permission_decision: false,
            grant_revoke: false,
            background_task_cancel: false,
            object_context: false,
        };

        expect(hasAiAssistantBrowserEntry(manager)).toBe(true);
        expect(manager.permission_decision).toBe(false);
        expect(manager.grant_revoke).toBe(false);
        expect(manager.background_task_cancel).toBe(false);
        expect(manager.object_context).toBe(false);
    });

    it('requires the complete minimum read-turn contract', () => {
        expect(hasAiAssistantBrowserEntry(null)).toBe(false);
        expect(hasAiAssistantBrowserEntry({
            ...OSS_AI_ASSISTANT_FEATURES,
            full_session_snapshot: false,
        })).toBe(false);
    });
});
