import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    deliverSignaling,
    flush,
    installSignalingStubs,
    latestSocket,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubWebSocket,
    type SignalingGlobals,
} from './file-transfer-test-harness';
import { deskErrorCodeEnum } from '@/services/types';

// A host still waiting for its manager credential proof answers
// `RequestRemoteAccess` with `ACTION_NEED_RETRY`. It is a "ask me again shortly",
// not a refusal, and the desk and terminal sessions have always treated it that
// way; the file manager did not, so a page opened during that window failed for
// no reason a user could act on.
//
// Retrying has to be bounded, or a host stuck in that state would keep the page
// asking forever, so both halves are asserted: the budget runs out and gives up,
// and a fresh attempt that gets a real answer succeeds. One comprehensive
// `renderHook` test per file: see the note in `file-transfer-test-harness.ts`.

const REQUEST_REMOTE_ACCESS = 100;
const REMOTE_ACCESS_INITIALIZED = 101;
const LIST_FILES = 10005;
const FILES_LISTED = 10015;
const RETRY_DELAY_MS = 500;

const needsRetry = {
    signaling_type: REMOTE_ACCESS_INITIALIZED,
    response_state: {
        error_code: deskErrorCodeEnum.ACTION_NEED_RETRY,
        message: 'awaiting manager credential proof',
    },
};

describe('useFileTransfer remote-access retry', () => {
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

    it('asks again on a bounded budget, gives up when it runs out, and succeeds when the host is ready', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));

        // --- The budget runs out ---
        let rejection: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).catch((error) => {
                rejection = error;
            });
        });
        await flush();
        const first = latestSocket();
        act(() => first.onopen?.());
        expect(sentSignalingOfType(first, REQUEST_REMOTE_ACCESS)).toHaveLength(1);

        for (let attempt = 1; attempt <= 3; attempt++) {
            await deliverSignaling(needsRetry);
            // The retry waits out its backoff rather than hammering the host.
            expect(sentSignalingOfType(first, REQUEST_REMOTE_ACCESS)).toHaveLength(attempt);
            await advanceTime(RETRY_DELAY_MS);
            expect(sentSignalingOfType(first, REQUEST_REMOTE_ACCESS)).toHaveLength(attempt + 1);
        }

        // Four asks in total, and the budget is spent: the next refusal is final.
        expect(sentSignalingOfType(first, REQUEST_REMOTE_ACCESS)).toHaveLength(4);
        await deliverSignaling(needsRetry);
        await advanceTime(RETRY_DELAY_MS);
        expect(sentSignalingOfType(first, REQUEST_REMOTE_ACCESS)).toHaveLength(4);
        expect(rejection).toBeInstanceOf(Error);
        expect((rejection as { code?: number }).code).toBe(deskErrorCodeEnum.ACTION_NEED_RETRY);
        expect(first.readyState).toBe(3);

        // --- A fresh attempt whose host is ready on the second ask succeeds ---
        let listed: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).then((response) => {
                listed = response;
            });
        });
        await flush();
        const second = latestSocket();
        expect(StubWebSocket.instances).toHaveLength(2);
        act(() => second.onopen?.());

        await deliverSignaling(needsRetry);
        await advanceTime(RETRY_DELAY_MS);
        expect(sentSignalingOfType(second, REQUEST_REMOTE_ACCESS)).toHaveLength(2);

        await deliverSignaling({
            signaling_type: REMOTE_ACCESS_INITIALIZED,
            signaling_data: { ice_servers: [], connection_epoch: 'test-epoch' },
        });
        await advanceTime(0);

        // The retry did not cost the caller its request: it goes out on the
        // session that finally came up, over the same socket.
        const ask = sentSignalingOfType(second, LIST_FILES).at(-1);
        expect(ask).toBeTruthy();
        await deliverSignaling({
            request_id: ask.request_id,
            signaling_type: FILES_LISTED,
            signaling_data: { file_info_list: [], total_count: 0 },
        });
        await advanceTime(0);
        expect(listed).toEqual({ file_info_list: [], total_count: 0 });
        expect(StubWebSocket.instances).toHaveLength(2);
    });
});
