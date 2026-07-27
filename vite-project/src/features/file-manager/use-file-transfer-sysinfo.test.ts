import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    installSignalingStubs,
    openChannel,
    parseSent,
    restoreSignalingStubs,
    flush,
    StubWebSocket,
    type SignalingGlobals,
} from './file-transfer-test-harness';
import { deskErrorCodeEnum } from '@/services/types';

// `querySystemInfo` asks the *host* what it is. The browser cannot read this
// from its own server: between them may sit a manager or a signaling server
// whose startup mode describes neither the host nor anything the file manager
// shows. The wire contract is what matters here — the request goes out as
// `ManagerSystemInfo` (10003) and its answer is matched back by request id —
// so that is what this asserts. One comprehensive `renderHook` test per file:
// see the note in `file-transfer-test-harness.ts`.

const MANAGER_SYSTEM_INFO = 10003;

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
        await openChannel();
        const ws = StubWebSocket.instances[StubWebSocket.instances.length - 1];
        await flush();

        const asked = ws.sent.map(parseSent).find(m => m.signaling_type === MANAGER_SYSTEM_INFO);
        expect(asked).toBeDefined();
        expect(asked.to_connection_id).toBe('desk-A');
        expect(asked.request_id).toBeTruthy();

        // --- The host's answer settles that exact request ---
        act(() => ws.onmessage?.({
            data: JSON.stringify({
                request_id: asked.request_id,
                signaling_type: MANAGER_SYSTEM_INFO,
                signaling_data: { startup_mode: 'service-daemon', host_name: 'alice-pc' },
            }),
        }));
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
            .filter(m => m.signaling_type === MANAGER_SYSTEM_INFO)
            .at(-1);
        expect(refused.request_id).not.toBe(asked.request_id);
        act(() => ws.onmessage?.({
            data: JSON.stringify({
                request_id: refused.request_id,
                signaling_type: MANAGER_SYSTEM_INFO,
                response_state: {
                    error_code: deskErrorCodeEnum.PERMISSION_ERROR,
                    message: 'the target is not reachable',
                },
            }),
        }));
        await flush();
        expect(rejection).toBeInstanceOf(Error);
        expect((rejection as Error).message).toBe('the target is not reachable');
    });
});
