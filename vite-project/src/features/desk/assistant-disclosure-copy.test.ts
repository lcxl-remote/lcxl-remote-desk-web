import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';

describe('assistant capability disclosure', () => {
    it('distinguishes draft handoff from separately confirmed sending in both languages', () => {
        expect(zh['pages.deviceAssistant.disclosure']).toContain('不能用草稿授权代替发送授权');
        expect(en['pages.deviceAssistant.disclosure']).toContain('draft authorization does not authorize sending');
        for (const locale of [zh, en]) {
            const disclosure = locale['pages.deviceAssistant.disclosure'];
            for (const app of ['Numbers', 'Pages', 'Keynote', 'Gmail', 'Slack']) expect(disclosure).toContain(app);
        }
    });

    it('retains command risk and avoids obsolete layout and developer wording', () => {
        expect(zh['pages.deviceAssistant.disclosure']).toContain('不保证只读');
        expect(en['pages.deviceAssistant.disclosure']).toContain('not guaranteed to be read-only');
        expect(zh['pages.deviceAssistant.disclosure']).not.toContain('下方显示');
        expect(en['pages.deviceAssistant.disclosure']).not.toContain('shown below');
        for (const locale of [zh, en]) {
            expect(locale['pages.deviceAssistant.sessionDescription']).not.toContain('daemon/worker');
            expect(locale['pages.deviceAssistant.providerBoundary']).toContain('{{model}}');
            expect(locale['pages.deviceAssistant.providerBoundary']).toContain('{{provider}}');
        }
    });
});
