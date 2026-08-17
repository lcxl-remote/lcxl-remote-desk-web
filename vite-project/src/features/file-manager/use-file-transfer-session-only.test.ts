import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DATA_CHANNEL_TIMEOUT_MS, useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    deliverSignaling,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    StubPeerConnection,
    type SignalingGlobals,
} from './file-transfer-test-harness';

// The degradation contract: when the data channel cannot be established, the
// signaling session survives it and everything that rides the session keeps
// working. Only file bytes become unavailable, and they say why.
//
// This is the guarantee that turns the production outage from "the page is
// unusable" into "you can browse but not transfer", so it is asserted on both
// sides: the channel fails and reports a relay-shaped diagnosis, and a listing
// issued *after* that failure still completes. One comprehensive `renderHook`
// test per file: see the note in `file-transfer-test-harness.ts`.

const LIST_FILES = 10005;
const FILES_LISTED = 10015;

describe('useFileTransfer when the data channel never opens', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
        // Installed before anything is scheduled: the channel timeout under test
        // is armed the moment the attempt starts.
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('keeps the session, reports the cause, and still lists files', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            result.current.prepareTransfers();
        });
        const ws = await openSession({ iceServers: [{ urls: 'turn:unreachable.example:3478' }] });

        const pc = StubPeerConnection.last!;
        // Local candidates gather fine; the relay never answers.
        act(() => pc.emitCandidate('candidate:1 1 udp 2113937151 192.168.1.5 50000 typ host'));

        await advanceTime(DATA_CHANNEL_TIMEOUT_MS);

        expect(result.current.channelStatus).toBe('failed');
        expect(result.current.channelFailure?.kind).toBe('channel-timeout');

        // The evidence points where it should: a relay was configured and not one
        // relay candidate came back.
        const diagnostics = result.current.channelFailure!.diagnostics;
        expect(diagnostics.iceServerUrls).toEqual(['turn:unreachable.example:3478']);
        expect(diagnostics.candidateCounts.host).toBe(1);
        expect(diagnostics.candidateCounts.relay).toBe(0);
        expect(diagnostics.failedStage).toBe('dataChannel');

        // The peer connection is gone. The socket is not — that is the split.
        expect(pc.closed).toBe(true);
        expect(ws.readyState).toBe(1);

        // A listing issued after the failure still completes.
        let listed: unknown = null;
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).then((response) => {
                listed = response;
            });
        });
        await advanceTime(0);
        const ask = sentSignalingOfType(ws, LIST_FILES).at(-1);
        expect(ask).toBeTruthy();
        await deliverSignaling({
            request_id: ask.request_id,
            signaling_type: FILES_LISTED,
            signaling_data: { file_info_list: [], total_count: 0 },
        });
        await advanceTime(0);
        expect(listed).toEqual({ file_info_list: [], total_count: 0 });

        // A transfer, by contrast, ends as an error rather than hanging.
        act(() => {
            void result.current.downloadFile('/a.txt', 'a.txt');
        });
        await advanceTime(DATA_CHANNEL_TIMEOUT_MS);
        const row = result.current.transfers.find((transfer) => transfer.fileName === 'a.txt');
        expect(row?.status).toBe('error');
    });
});
