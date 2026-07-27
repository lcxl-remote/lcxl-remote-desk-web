import { describe, expect, it } from 'vitest';
import type { TFunction } from 'i18next';

import { deskErrorCodeEnum } from '@/services/types';
import {
    deskErrorKey,
    deskErrorKeyOr,
    deskErrorMessage,
    errorCodeOf,
    type ErrorCodeKeyMap,
} from './desk-error-i18n';

// Echoes the key back, so an assertion can tell "localized this key" apart from
// "passed the backend text through".
const t = ((key: string) => `t:${key}`) as unknown as TFunction;

const MAP: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PERMISSION_ERROR]: 'keys.denied',
    [deskErrorCodeEnum.REVISION_CONFLICT]: 'keys.conflict',
};

describe('deskErrorKey', () => {
    it('resolves a mapped code', () => {
        expect(deskErrorKey(MAP, deskErrorCodeEnum.PERMISSION_ERROR)).toBe('keys.denied');
    });

    it('is undefined for an unmapped code or a missing one', () => {
        expect(deskErrorKey(MAP, deskErrorCodeEnum.SYSTEM_ERROR)).toBeUndefined();
        expect(deskErrorKey(MAP, null)).toBeUndefined();
        expect(deskErrorKey(MAP, undefined)).toBeUndefined();
    });

    // SUCCESS is 0, which is falsy — a lookup guarded by truthiness would treat a
    // mapped SUCCESS as unmapped.
    it('resolves code 0 like any other', () => {
        const withSuccess: ErrorCodeKeyMap = { [deskErrorCodeEnum.SUCCESS]: 'keys.ok' };
        expect(deskErrorKey(withSuccess, deskErrorCodeEnum.SUCCESS)).toBe('keys.ok');
    });
});

describe('deskErrorKeyOr', () => {
    it('falls back to the generic key, never to the backend message', () => {
        expect(deskErrorKeyOr(MAP, 999, 'keys.generic')).toBe('keys.generic');
        expect(deskErrorKeyOr(MAP, null, 'keys.generic')).toBe('keys.generic');
        expect(deskErrorKeyOr(MAP, deskErrorCodeEnum.REVISION_CONFLICT, 'keys.generic')).toBe(
            'keys.conflict',
        );
    });
});

describe('deskErrorMessage', () => {
    it('localizes a mapped code instead of showing the backend text', () => {
        expect(
            deskErrorMessage(t, MAP, deskErrorCodeEnum.PERMISSION_ERROR, 'File delete access denied', 'fallback'),
        ).toBe('t:keys.denied');
    });

    it('passes the backend message through for an unmapped code', () => {
        expect(
            deskErrorMessage(t, MAP, deskErrorCodeEnum.SYSTEM_ERROR, 'Failed to move to trash: EBUSY', 'fallback'),
        ).toBe('Failed to move to trash: EBUSY');
    });

    it('uses the fallback when there is neither a mapped code nor a message', () => {
        expect(deskErrorMessage(t, MAP, undefined, '', 'fallback')).toBe('fallback');
        expect(deskErrorMessage(t, MAP, undefined, null, 'fallback')).toBe('fallback');
    });
});

describe('errorCodeOf', () => {
    it('reads the code off an error that carries one', () => {
        class Carrier extends Error {
            readonly code = deskErrorCodeEnum.PERMISSION_ERROR;
        }
        expect(errorCodeOf(new Carrier('denied'))).toBe(deskErrorCodeEnum.PERMISSION_ERROR);
    });

    it('is undefined for a plain error, a non-error, or a non-numeric code', () => {
        expect(errorCodeOf(new Error('boom'))).toBeUndefined();
        expect(errorCodeOf('boom')).toBeUndefined();
        expect(errorCodeOf(null)).toBeUndefined();
        expect(errorCodeOf(Object.assign(new Error('boom'), { code: 'ENOENT' }))).toBeUndefined();
    });

    // A rejected transfer reaches `catch` as `unknown`, and code 0 is falsy.
    it('reads a zero code rather than reporting none', () => {
        expect(errorCodeOf(Object.assign(new Error('ok'), { code: 0 }))).toBe(0);
    });
});
