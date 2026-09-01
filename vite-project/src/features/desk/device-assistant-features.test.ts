import { describe, expect, it } from 'vitest';

import {
    OSS_DEVICE_ASSISTANT_FEATURES,
    hasDeviceAssistantBrowserEntry,
} from './device-assistant-features';

describe('Device Assistant feature profile', () => {
    it('keeps the Manager browser entry while independently gating unfinished controls', () => {
        const manager = {
            ...OSS_DEVICE_ASSISTANT_FEATURES,
            permission_decision: false,
            grant_revoke: false,
            background_task_cancel: false,
            object_context: false,
        };

        expect(hasDeviceAssistantBrowserEntry(manager)).toBe(true);
        expect(manager.permission_decision).toBe(false);
        expect(manager.grant_revoke).toBe(false);
        expect(manager.background_task_cancel).toBe(false);
        expect(manager.object_context).toBe(false);
    });

    it('requires the complete minimum read-turn contract', () => {
        expect(hasDeviceAssistantBrowserEntry(null)).toBe(false);
        expect(hasDeviceAssistantBrowserEntry({
            ...OSS_DEVICE_ASSISTANT_FEATURES,
            full_session_snapshot: false,
        })).toBe(false);
    });
});
