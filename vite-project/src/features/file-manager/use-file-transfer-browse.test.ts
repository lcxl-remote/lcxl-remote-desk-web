import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import { installSignalingStubs, restoreSignalingStubs, openSession, deliverSignaling, sentSignalingOfType, flush, type SignalingGlobals } from './file-transfer-test-harness';
import { SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS, SIGNALING_TYPE_CODE_LIST_FILES, SIGNALING_TYPE_CODE_FILES_LISTED, SIGNALING_TYPE_CODE_OFFER, SIGNALING_TYPE_CODE_ICE_CANDIDATE } from '@/features/desk/constants';

let saved: SignalingGlobals;
beforeEach(() => { saved = installSignalingStubs(); });
afterEach(() => { cleanup(); restoreSignalingStubs(saved); });

it.each(['target-A', null])('browses fixed target %s without WebRTC', async (target) => {
    const construct = vi.fn(function () { throw new Error('WebRTC must not be used'); });
    globalThis.RTCPeerConnection = construct as unknown as typeof RTCPeerConnection;
    const { result } = renderHook(() => useFileTransfer('desk-A', undefined, target));
    let pending!: Promise<unknown>;
    act(() => { pending = result.current.listFiles({ path: '/', page_no: 1, page_count: 100, directories_only: true }); });
    const ws = await openSession();
    await flush();
    expect(sentSignalingOfType(ws, SIGNALING_TYPE_CODE_REQUEST_REMOTE_ACCESS)[0].signaling_data.session_target_id).toBe(target ?? undefined);
    const request = sentSignalingOfType(ws, SIGNALING_TYPE_CODE_LIST_FILES)[0];
    expect(request.signaling_data.directories_only).toBe(true);
    await deliverSignaling({ request_id: request.request_id, signaling_type: SIGNALING_TYPE_CODE_FILES_LISTED, signaling_data: { file_info_list: [], total_count: 0 } });
    await expect(pending).resolves.toEqual({ file_info_list: [], total_count: 0 });
    expect(construct).not.toHaveBeenCalled();
    expect(sentSignalingOfType(ws, SIGNALING_TYPE_CODE_OFFER)).toHaveLength(0);
    expect(sentSignalingOfType(ws, SIGNALING_TYPE_CODE_ICE_CANDIDATE)).toHaveLength(0);
    act(() => result.current.closeConnection());
    expect(ws.readyState).not.toBe(WebSocket.OPEN);
});
