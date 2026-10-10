import { describe, expect, it } from 'vitest';
import { parseOutputTokens } from './output-token-settings';

describe('output token settings', () => {
    it('accepts the complete positive u32 range and rejects invalid input', () => {
        expect(parseOutputTokens('32768')).toBe(32768);
        expect(parseOutputTokens('4294967295')).toBe(4294967295);
        for (const value of ['', ' ', '0', '-1', '1.5', '4294967296', 'Infinity', '1e3']) {
            expect(parseOutputTokens(value)).toBeNull();
        }
    });
});
