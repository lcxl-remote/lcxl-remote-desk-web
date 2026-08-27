import {
    SIGNALING_TYPE_CODE_CANCEL_COMPUTER_ACTION,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_COMPLETED,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_STARTED,
    SIGNALING_TYPE_CODE_COMPUTER_ACTION_STATE_REPORTED,
    SIGNALING_TYPE_CODE_DISPATCH_COMPUTER_ACTION,
    SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_STATE,
} from './constants';

export type ComputerActionResponseEnvelope = {
    request_id: string;
    signaling_type: number;
};

/**
 * Correlates Computer Action lifecycle frames without allowing a stale request
 * id or a response belonging to another command role to complete the pending
 * operation.
 */
export function matchesComputerActionResponse(
    pendingRequestId: string,
    requestType: number,
    response: ComputerActionResponseEnvelope,
): boolean {
    if (response.request_id !== pendingRequestId) return false;
    if (requestType === SIGNALING_TYPE_CODE_DISPATCH_COMPUTER_ACTION) {
        return response.signaling_type === SIGNALING_TYPE_CODE_COMPUTER_ACTION_STARTED
            || response.signaling_type === SIGNALING_TYPE_CODE_COMPUTER_ACTION_COMPLETED;
    }
    if (requestType === SIGNALING_TYPE_CODE_CANCEL_COMPUTER_ACTION
        || requestType === SIGNALING_TYPE_CODE_QUERY_COMPUTER_ACTION_STATE) {
        return response.signaling_type === SIGNALING_TYPE_CODE_COMPUTER_ACTION_STATE_REPORTED;
    }
    return false;
}
