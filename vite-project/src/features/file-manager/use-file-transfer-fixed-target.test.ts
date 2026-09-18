import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import { installSignalingStubs, restoreSignalingStubs, latestSocket, deliverSignaling, sentSignalingOfType, flush, type SignalingGlobals } from './file-transfer-test-harness';
import { SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS, SIGNALING_TYPE_CODE_LIST_FILES, SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED } from '@/features/desk/constants';
import { deskErrorCodeEnum } from '@/services/types';

let saved: SignalingGlobals;
beforeEach(() => { saved = installSignalingStubs(); });
afterEach(() => { cleanup(); restoreSignalingStubs(saved); });

it.each(['old-target', null])('refuses stale fixed target %s without switching sessions', async (target) => {
    const { result } = renderHook(() => useFileTransfer('desk-A', undefined, target));
    let pending!: Promise<unknown>;
    act(() => { pending = result.current.listFiles({ path: '/', page_no: 1, page_count: 100, directories_only: true }).catch(error => error); });
    await flush();
    const ws = latestSocket();
    act(() => ws.onopen?.());
    await deliverSignaling({ signaling_type: SIGNALING_TYPE_CODE_REMOTE_ACCESS_INITIALIZED,
        response_state: { error_code: deskErrorCodeEnum.SESSION_TARGET_STALE, message: 'Target no longer exists' },
        signaling_data: { targets: [{ target_id: 'different-user', display_name: 'Another session' }] } });
    const error = await pending;
    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toContain('Target no longer exists');
    expect(sentSignalingOfType(ws, SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS)).toHaveLength(1);
    expect(sentSignalingOfType(ws, SIGNALING_TYPE_CODE_LIST_FILES)).toHaveLength(0);
    expect(ws.readyState).not.toBe(WebSocket.OPEN);
});
