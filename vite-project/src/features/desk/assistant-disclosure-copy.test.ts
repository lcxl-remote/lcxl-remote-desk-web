import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';

describe('assistant capability disclosure', () => {
    it('removes redundant general notices', () => {
        for (const locale of [zh, en]) {
            expect(Object.keys(locale)).not.toContain('pages.deviceAssistant.workspace.reviewNotice');
            expect(Object.keys(locale)).not.toContain('pages.deviceAssistant.disclosure');
            expect(Object.keys(locale)).not.toContain('pages.deviceAssistant.disclosureTitle');
        }
    });
    it('avoids obsolete layout and developer wording', () => {
        for (const locale of [zh, en]) {
            expect(locale['pages.deviceAssistant.sessionDescription']).not.toContain('daemon/worker');
            expect(locale['pages.deviceAssistant.providerBoundary']).toContain('{{model}}');
            expect(locale['pages.deviceAssistant.providerBoundary']).toContain('{{provider}}');
        }
    });
});
