import { describe, expect, it } from 'vitest';

import { isDeviceAssistantEnabled } from './device-assistant-switch';

describe('isDeviceAssistantEnabled', () => {
    it('requires an explicit enabled projection', () => {
        expect(isDeviceAssistantEnabled({ device_assistant_enabled: true })).toBe(true);
        expect(isDeviceAssistantEnabled({ device_assistant_enabled: false })).toBe(false);
        expect(isDeviceAssistantEnabled({})).toBe(false);
        expect(isDeviceAssistantEnabled(null)).toBe(false);
    });
});
