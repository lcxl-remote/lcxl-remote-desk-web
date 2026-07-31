import { describe, expect, it } from 'vitest';
import type { TFunction } from 'i18next';

import { deskErrorCodeEnum } from '@/services/types';
import { agentErrorMessage } from './agent-error-i18n';

const t = ((key: string) => `t:${key}`) as unknown as TFunction;

describe('agentErrorMessage', () => {
    it.each([
        [deskErrorCodeEnum.RATE_LIMITED, 'pages.agentError.aiRateDisabled'],
        [
            deskErrorCodeEnum.AI_CONTEXT_LIMIT_EXCEEDED,
            'pages.agentError.aiContextLimitExceeded',
        ],
        [deskErrorCodeEnum.AI_PLATFORM_BUSY, 'pages.agentError.aiPlatformBusy'],
    ])('localizes platform AI safeguard code %s', (code, key) => {
        expect(agentErrorMessage(t, code, 'raw backend text', 'fallback')).toBe(`t:${key}`);
    });
});
