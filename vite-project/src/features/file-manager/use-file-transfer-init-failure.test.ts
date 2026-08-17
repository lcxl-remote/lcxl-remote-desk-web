import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    deliverSignaling,
    flush,
    installSignalingStubs,
    latestSocket,
    restoreSignalingStubs,
    type SignalingGlobals,
} from './file-transfer-test-harness';
import { deskErrorCodeEnum } from '@/services/types';

// A host that refuses the session answers `RemoteAccessInitialized` with an error
// state and no payload. That state was never inspected: the page went straight
// for `signaling_data.ice_servers`, and the refusal reached the user — if at all
// — as a generic timeout twenty seconds later.
//
// Fake timers are installed before anything is scheduled and never advanced past
// zero, so "rejects immediately" is not a matter of interpretation here: no
// timeout could have fired. One comprehensive `renderHook` test per file: see the
// note in `file-transfer-test-harness.ts`.

describe('useFileTransfer when the host refuses the session', () => {
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

    it("rejects with the host's own code and closes the socket, without waiting", async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));

        let rejection: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '', page_no: 1, page_count: 100 }).catch((error) => {
                rejection = error;
            });
        });
        await flush();

        const ws = latestSocket();
        act(() => ws.onopen?.());
        await deliverSignaling({
            signaling_type: 101, // REMOTE_ACCESS_INITIALIZED
            signaling_data: null,
            response_state: {
                error_code: deskErrorCodeEnum.PERMISSION_ERROR,
                message: 'not permitted',
            },
        });
        await flush();

        // The caller learns what the host said, not that something timed out.
        expect(rejection).toBeInstanceOf(Error);
        expect((rejection as { code?: number }).code).toBe(deskErrorCodeEnum.PERMISSION_ERROR);
        expect((rejection as Error).message).toBe('not permitted');

        // Virtual time never moved past zero, so nothing here came from a timeout —
        // and nothing is left armed to fire later.
        expect(vi.getTimerCount()).toBe(0);

        // The socket is released rather than left open until the page is closed.
        expect(ws.readyState).toBe(3);
    });
});
