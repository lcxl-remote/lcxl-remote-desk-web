import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import { flush, installSignalingStubs, openSession, restoreSignalingStubs, StubDataChannel, type SignalingGlobals } from './__tests__/harness';

let saved: SignalingGlobals;
beforeEach(() => { saved = installSignalingStubs(); });
afterEach(() => { cleanup(); restoreSignalingStubs(saved); });

it('cancels before connection setup completes without starting a file transfer afterwards', async () => {
    const { result } = renderHook(() => useFileTransfer('host'));
    const stream = vi.fn();
    const file = { name: 'never-read', size: 10, stream } as unknown as File;
    let upload!: Promise<void>;
    act(() => { upload = result.current.uploadFile('/downloads', file); });
    act(() => { result.current.cancelTransfer(result.current.transfers[0].transferId); });
    await openSession();
    await flush();
    await upload;
    expect(result.current.transfers[0].status).toBe('cancelled');
    expect(StubDataChannel.last).toBeNull();
    expect(stream).not.toHaveBeenCalled();
});
