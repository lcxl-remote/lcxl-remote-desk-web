import { describe, expect, it } from 'vitest';

import {
    SIGNALING_API_VERSION,
    SIGNALING_TYPE_CODE_CANCEL_COMPUTER_ACTION,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_COMPLETED,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_STARTED,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_STATE_REPORTED,
    SIGNALING_TYPE_CODE_COMPUTER_USE_READINESS_UPDATED,
    SIGNALING_TYPE_CODE_DISPATCH_COMPUTER_ACTION,
    SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_STATE,
} from './constants';
import { matchesComputerActionResponse } from './computer-action-protocol';

describe('Computer Action signaling protocol', () => {
    it('keeps the shared discriminants stable', () => {
        expect(SIGNALING_API_VERSION).toBe(1);
        expect([
            SIGNALING_TYPE_CODE_DISPATCH_COMPUTER_ACTION,
            SIGNALING_TYPE_CODE_CANCEL_COMPUTER_ACTION,
            SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_STATE,
            SIGNALING_TYPE_CODE_COMPUTER_ACTION_STARTED,
            SIGNALING_TYPE_CODE_COMPUTER_ACTION_COMPLETED,
            SIGNALING_TYPE_CODE_COMPUTER_ACTION_STATE_REPORTED,
            SIGNALING_TYPE_CODE_COMPUTER_USE_READINESS_UPDATED,
        ]).toEqual([626, 627, 628, 629, 630, 631, 632]);
    });

    it('rejects stale request ids and responses for another command role', () => {
        expect(matchesComputerActionResponse('current', SIGNALING_TYPE_CODE_DISPATCH_COMPUTER_ACTION, {
            request_id: 'stale',
            signaling_type: SIGNALING_TYPE_CODE_COMPUTER_ACTION_STARTED,
        })).toBe(false);
        expect(matchesComputerActionResponse('current', SIGNALING_TYPE_CODE_CANCEL_COMPUTER_ACTION, {
            request_id: 'current',
            signaling_type: SIGNALING_TYPE_CODE_COMPUTER_ACTION_COMPLETED,
        })).toBe(false);
        expect(matchesComputerActionResponse('current', SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_STATE, {
            request_id: 'current',
            signaling_type: SIGNALING_TYPE_CODE_COMPUTER_ACTION_STATE_REPORTED,
        })).toBe(true);
    });
});
