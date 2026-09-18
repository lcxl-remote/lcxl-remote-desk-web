import { describe, expect, it } from 'vitest';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';

describe('assistant capability disclosure', () => {
    it('removes redundant general notices', () => {
        for (const locale of [zh, en]) {
            expect(Object.keys(locale)).not.toContain('pages.aiAssistant.workspace.reviewNotice');
            expect(Object.keys(locale)).not.toContain('pages.aiAssistant.disclosure');
            expect(Object.keys(locale)).not.toContain('pages.aiAssistant.disclosureTitle');
        }
    });
    it('avoids obsolete layout and developer wording', () => {
        for (const locale of [zh, en]) {
            expect(locale['pages.aiAssistant.sessionDescription']).not.toContain('daemon/worker');
            expect(locale['pages.aiAssistant.providerBoundary']).toContain('{{model}}');
            expect(locale['pages.aiAssistant.providerBoundary']).toContain('{{provider}}');
        }
    });
});
