import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    deliverSignaling,
    flush,
    installSignalingStubs,
    latestSocket,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubPeerConnection,
    type SignalingGlobals,
} from './file-transfer-test-harness';
import { deskErrorCodeEnum } from '@/services/types';

// A request the host rejects before its own handler runs — the door1 gate — comes
// back as the protocol-level `Error` frame rather than as the request's declared
// response type. The page ignored those frames entirely, so such a rejection was
// indistinguishable from silence and every one of them ended as a timeout.
//
// Which timeout it ended as depends on what the frame was answering, so all three
// cases are pinned here: a rejected request, a rejected data-plane frame, and a
// rejected session. The middle one matters most — a data-plane rejection must not
// take browsing down with it. One comprehensive `renderHook` test per file: see
// the note in `file-transfer-test-harness.ts`.

const ERROR_FRAME = -1;
const LIST_FILES = 10005;

describe('useFileTransfer error frames', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('rejects the matching request, fails only the channel, and fails the session when it is the session that is refused', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            result.current.prepareTransfers();
        });
        const ws = await openSession({ iceServers: [{ urls: 'stun:stun.example:3478' }] });

        // --- A rejected request fails that request, and nothing else ---
        let rejection: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).catch((error) => {
                rejection = error;
            });
        });
        await advanceTime(0);
        const ask = sentSignalingOfType(ws, LIST_FILES).at(-1);
        expect(ask).toBeTruthy();

        await deliverSignaling({
            request_id: ask.request_id,
            signaling_type: ERROR_FRAME,
            response_state: {
                error_code: deskErrorCodeEnum.PERMISSION_ERROR,
                message: 'This connection is not permitted to perform the requested action',
            },
        });
        await advanceTime(0);

        expect(rejection).toBeInstanceOf(Error);
        expect((rejection as { code?: number }).code).toBe(deskErrorCodeEnum.PERMISSION_ERROR);
        // The session is untouched: one refused request is not a broken connection.
        expect(ws.readyState).toBe(1);
        expect(result.current.channelStatus).not.toBe('failed');

        // --- A rejected data-plane frame fails the channel, not the session ---
        await deliverSignaling({
            request_id: 'offer-request-id',
            signaling_type: ERROR_FRAME,
            response_state: {
                error_code: deskErrorCodeEnum.REMOTE_ACCESS_LOCKED,
                message: 'Remote access is locked on the host',
            },
        });
        await advanceTime(0);

        expect(result.current.channelStatus).toBe('failed');
        expect(result.current.channelFailure?.errorCode).toBe(deskErrorCodeEnum.REMOTE_ACCESS_LOCKED);
        expect(StubPeerConnection.last!.closed).toBe(true);
        // Browsing survives it — that is the whole point of the split.
        expect(ws.readyState).toBe(1);
        let listed: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).then((response) => {
                listed = response;
            });
        });
        await advanceTime(0);
        const second = sentSignalingOfType(ws, LIST_FILES).at(-1);
        await deliverSignaling({
            request_id: second.request_id,
            signaling_type: 10015, // FILES_LISTED
            signaling_data: { file_info_list: [], total_count: 0 },
        });
        await advanceTime(0);
        expect(listed).toEqual({ file_info_list: [], total_count: 0 });

        // --- An error answering the session request fails the session ---
        // The next request has to build a new session, and this time the host
        // rejects the handshake outright.
        act(() => {
            result.current.closeConnection();
        });
        let sessionRejection: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).catch((error) => {
                sessionRejection = error;
            });
        });
        await flush();
        const fresh = latestSocket();
        act(() => fresh.onopen?.());
        await deliverSignaling({
            request_id: 'request-remote-access-id',
            signaling_type: ERROR_FRAME,
            response_state: {
                error_code: deskErrorCodeEnum.REMOTE_ACCESS_LOCKED,
                message: 'Remote access is locked on the host',
            },
        });
        await advanceTime(0);

        expect(sessionRejection).toBeInstanceOf(Error);
        expect((sessionRejection as { code?: number }).code).toBe(deskErrorCodeEnum.REMOTE_ACCESS_LOCKED);
        expect(fresh.readyState).toBe(3);
    });
});
