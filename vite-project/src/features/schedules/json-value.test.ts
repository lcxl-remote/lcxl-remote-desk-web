import { describe, expect, it } from 'vitest';
import { sameJsonValue } from './json-value';

describe('schedule JSON values', () => {
    it('ignores object member order at every level', () => {
        expect(sameJsonValue({ schema_version: 1, rule: { kind: 'daily', utc_time: '08:00:00' } },
            { rule: { utc_time: '08:00:00', kind: 'daily' }, schema_version: 1 })).toBe(true);
    });
    it('preserves array order, field presence, scalar types and exact business text', () => {
        for (const [left, right] of [
            [[1, 2], [2, 1]], [{ value: 1 }, { value: '1' }], [{ value: null }, {}],
            [{ body: 'a1' }, { body: 'native-token' }], [{ body: 'a b' }, { body: 'ab' }],
            [{ rule: { kind: 'daily' } }, { rule: { kind: 'once' } }],
        ]) expect(sameJsonValue(left, right)).toBe(false);
    });
});
