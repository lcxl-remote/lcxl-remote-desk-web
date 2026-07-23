import { beforeEach, describe, expect, it } from 'vitest';

import i18n, {
    canonicalizeLocale,
    ensureLocaleLoaded,
    initializeI18n,
} from './i18n';

beforeEach(() => {
    localStorage.clear();
});

describe('locale loading', () => {
    it('loads only the saved locale during startup', async () => {
        localStorage.setItem('i18nextLng', 'en-US');

        await initializeI18n();

        expect(i18n.resolvedLanguage).toBe('en-US');
        expect(i18n.hasResourceBundle('en-US', 'translation')).toBe(true);
        expect(i18n.hasResourceBundle('zh-CN', 'translation')).toBe(false);
    });

    it('loads another base locale without overwriting extensions', async () => {
        await initializeI18n();
        i18n.addResourceBundle(
            'zh-CN',
            'translation',
            { extension: { label: '扩展文案' } },
            true,
            true,
        );
        await ensureLocaleLoaded('zh-CN');

        expect(i18n.hasResourceBundle('zh-CN', 'translation')).toBe(true);
        expect(i18n.getResource('zh-CN', 'translation', 'navBar.lang')).toBe(
            '语言',
        );
        expect(i18n.getResource('zh-CN', 'translation', 'extension.label')).toBe(
            '扩展文案',
        );
    });
});

describe('canonicalizeLocale', () => {
    it.each([
        ['en', 'en-US'],
        ['EN_us', 'en-US'],
        ['zh-Hans', 'zh-CN'],
        ['zh_cn', 'zh-CN'],
        ['fr-FR', null],
    ])('maps %s to %s', (input, expected) => {
        expect(canonicalizeLocale(input)).toBe(expected);
    });
});
