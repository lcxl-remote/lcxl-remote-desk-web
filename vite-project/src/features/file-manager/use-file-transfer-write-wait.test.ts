import { renderHook, act, cleanup } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useFileTransfer } from './use-file-transfer';
import {
    advanceTime,
    deliverSignaling,
    installSignalingStubs,
    openSession,
    restoreSignalingStubs,
    sentSignalingOfType,
    type SignalingGlobals,
} from './__tests__/harness';

// File writes may wait on the host user's approval for as long as the host's
// approval timeout allows. Giving up after a fixed client timeout would report a
// failure for a folder the host then creates, so writes wait for their reply or
// for the session to close. Listing keeps its timeout. One comprehensive
// `renderHook` test per file: see the note in `__tests__/harness.ts`.

const CREATE_DIRECTORY = 10018;
const DIRECTORY_CREATED = 10019;
const DELETE_FILE = 10006;
const FILE_DELETED = 10016;
const LIST_FILES = 10005;

describe('useFileTransfer write requests', () => {
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

    it('waits past the listing timeout, matches only its own reply, and ends when the session closes', async () => {
        const { result } = renderHook(() => useFileTransfer('desk-A'));
        act(() => {
            void result.current.listFiles({ path: '/', page_no: 1, page_count: 100 }).catch(() => { });
        });
        const ws = await openSession({ iceServers: [] });

        let created: 'pending' | 'done' | Error = 'pending';
        act(() => {
            void result.current.createDirectory({ parent_path: '/srv', name: 'new' })
                .then(() => { created = 'done'; }, (error: Error) => { created = error; });
        });
        let deleted: 'pending' | Error = 'pending';
        act(() => {
            void result.current.deleteFile({ file_path: '/srv/old', delete_permanently: false })
                .catch((error: Error) => { deleted = error; });
        });
        await advanceTime(0);
        const create = sentSignalingOfType(ws, CREATE_DIRECTORY).at(-1);
        expect(create.signaling_data).toEqual({ parent_path: '/srv', name: 'new' });
        expect(sentSignalingOfType(ws, DELETE_FILE)).toHaveLength(1);
        expect(sentSignalingOfType(ws, LIST_FILES)).toHaveLength(1);

        // Well past the 30 s listing timeout, the writes are still waiting.
        await advanceTime(120_000);
        expect(created).toBe('pending');
        expect(deleted).toBe('pending');

        // A reply of the wrong type, or for another request, completes nothing.
        await deliverSignaling({ request_id: create.request_id, signaling_type: FILE_DELETED });
        await deliverSignaling({ request_id: 'stale-request', signaling_type: DIRECTORY_CREATED });
        await advanceTime(0);
        expect(created).toBe('pending');

        await deliverSignaling({ request_id: create.request_id, signaling_type: DIRECTORY_CREATED });
        await advanceTime(0);
        expect(created).toBe('done');

        // Closing the session ends the delete that is still waiting.
        act(() => {
            result.current.closeConnection();
        });
        await advanceTime(0);
        expect(deleted).toBeInstanceOf(Error);
    });
});
