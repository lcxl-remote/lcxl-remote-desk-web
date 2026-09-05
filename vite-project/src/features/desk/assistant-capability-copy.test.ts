import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';
import { capabilityDescriptionKey } from './assistant-capability-copy';

describe('capability inventory copy', () => {
    it('uses an explicit fallback for unknown key namespaces', () => {
        expect(capabilityDescriptionKey('future.capability')).toBe('pages.deviceAssistant.workspace.descriptionUnavailable');
        expect(capabilityDescriptionKey('assistant.capability.systemCommandExecute'))
            .toBe('assistant.capabilityDescription.systemCommandExecute');
    });
    it('provides a name and description in both languages for every registered capability', () => {
        const source = readFileSync('../diagnose-core/src/device_assistant.rs', 'utf8');
        const keys = [...new Set([...source.matchAll(/"(assistant\.capability\.[A-Za-z]+)"/g)].map((match) => match[1]))];
        expect(keys.length).toBeGreaterThan(0);
        for (const key of keys) {
            for (const locale of [zh, en]) {
                const copy = locale as Record<string, string>;
                expect(copy[key], key).toBeTruthy();
                expect(copy[key.replace('assistant.capability.', 'assistant.capabilityDescription.')], key).toBeTruthy();
            }
        }
    });
});
