import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    deliverSignaling,
    installSignalingStubs,
    openSession,
    parseSent,
    restoreSignalingStubs,
    flush,
    StubPeerConnection,
    StubWebSocket,
    type SignalingGlobals,
} from './file-transfer-test-harness';
import { deskErrorCodeEnum } from '@/services/types';

// `querySystemInfo` asks the *host* what it is. The browser cannot read this
// from its own server: between them may sit a manager or a signaling server
// whose startup mode describes neither the host nor anything the file manager
// shows. The wire contract is what matters here — the request goes out as
// `GetSystemInfo` (10003) and its `SystemInfoRetrieved` response are matched by request id —
// so that is what this asserts. It rides the signaling session alone, which is
// why the handshake here stops before any data channel. One comprehensive
// `renderHook` test per file: see the note in `file-transfer-test-harness.ts`.

const GET_SYSTEM_INFO = 10003;
const SYSTEM_INFO_RETRIEVED = 10004;

describe('useFileTransfer host system info', () => {
    let saved: SignalingGlobals;

    beforeEach(() => {
        saved = installSignalingStubs();
    });

    afterEach(() => {
        cleanup();
        restoreSignalingStubs(saved);
    });

    it('asks the host over signaling, resolves with its answer, and rejects a refusal', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        const api = result.current;

        // --- The question reaches the host, addressed to it and not the server ---
        // Asking is also what opens the connection, so the request goes first.
        let resolved: unknown = null;
        act(() => {
            void api.querySystemInfo().then(info => {
                resolved = info;
            });
        });
        await openSession();
        const ws = StubWebSocket.instances[StubWebSocket.instances.length - 1];
        await flush();

        // The signaling session alone carries this: no peer connection is built,
        // so a host the browser cannot reach over WebRTC can still be asked.
        expect(StubPeerConnection.instances).toHaveLength(0);

        const asked = ws.sent.map(parseSent).find(m => m.signaling_type === GET_SYSTEM_INFO);
        expect(asked).toBeDefined();
        expect(asked.to_connection_id).toBe('desk-A');
        expect(asked.request_id).toBeTruthy();

        // --- The host's answer settles that exact request ---
        await deliverSignaling({
            request_id: asked.request_id,
            signaling_type: SYSTEM_INFO_RETRIEVED,
            signaling_data: { startup_mode: 'service-daemon', host_name: 'alice-pc' },
        });
        await flush();
        expect(resolved).toEqual({ startup_mode: 'service-daemon', host_name: 'alice-pc' });

        // --- A refusal rejects rather than hanging or resolving to nothing ---
        // A session holding a capped grant may not read the host's system
        // information, so callers have to see the failure and treat the mode as
        // unknown; a silent `undefined` would read as "not a service daemon".
        let rejection: unknown = null;
        act(() => {
            void api.querySystemInfo().catch(error => {
                rejection = error;
            });
        });
        await flush();

        const refused = ws.sent
            .map(parseSent)
            .filter(m => m.signaling_type === GET_SYSTEM_INFO)
            .at(-1);
        expect(refused.request_id).not.toBe(asked.request_id);
        await deliverSignaling({
            request_id: refused.request_id,
            signaling_type: SYSTEM_INFO_RETRIEVED,
            response_state: {
                error_code: deskErrorCodeEnum.PERMISSION_ERROR,
                message: 'the target is not reachable',
            },
        });
        await flush();
        expect(rejection).toBeInstanceOf(Error);
        expect((rejection as Error).message).toBe('the target is not reachable');
    });
});
