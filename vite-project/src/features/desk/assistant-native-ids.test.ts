import { describe, expect, it } from 'vitest';
import { assistantMessageId } from './assistant-native-ids';

describe('native assistant message identities', () => {
    it('encodes all bytes using the shared URL-safe representation', () => {
        const bytes = Uint8Array.from({ length: 16 }, (_, i) => i);
        expect(assistantMessageId(bytes)).toBe('m_AAECAwQFBgcICQoLDA0ODw');
        expect(assistantMessageId(bytes)).toHaveLength(24);
        const different = bytes.slice();
        different[15] = 255;
        expect(assistantMessageId(different)).not.toBe(assistantMessageId(bytes));
    });

    it('rejects incomplete identity material', () => {
        expect(() => assistantMessageId(new Uint8Array(15))).toThrow();
    });
});
