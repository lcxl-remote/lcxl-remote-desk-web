import { beforeEach, describe, expect, it, vi } from 'vitest';

const localeMock = vi.hoisted(() => ({
    attempts: 0,
}));

vi.mock('./zh-CN', () => ({
    get default() {
        localeMock.attempts += 1;
        if (localeMock.attempts === 1) {
            throw new Error('locale chunk failed');
        }
        return {
            'navBar.lang': '语言',
        };
    },
}));

import i18n, { ensureLocaleLoaded, initializeI18n } from './i18n';

beforeEach(() => {
    localStorage.clear();
    localStorage.setItem('i18nextLng', 'en-US');
});

describe('locale resource retries', () => {
    it('does not retain a failed locale loading promise', async () => {
        await initializeI18n();

        await expect(ensureLocaleLoaded('zh-CN')).rejects.toThrow(
            'locale chunk failed',
        );
        await ensureLocaleLoaded('zh-CN');

        expect(localeMock.attempts).toBe(2);
        expect(i18n.getResource('zh-CN', 'translation', 'navBar.lang')).toBe(
            '语言',
        );
    });
});
