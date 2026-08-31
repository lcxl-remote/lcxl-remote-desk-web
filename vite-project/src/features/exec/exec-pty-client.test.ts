import { describe, expect, it } from 'vitest';

import { execPtyWireForTest } from './exec-pty-client';

describe('exec PTY wire', () => {
    it('keeps opaque input outside metadata and round-trips arbitrary bytes', () => {
        const canary = new Uint8Array([0, 0xff, 0x80, 0x41, 0x0a]);
        const frame = execPtyWireForTest.encodeFrame(1, {
            stream_id: 'stream-1',
            execution_generation: 'generation-1',
            session_target_id: 'session-1',
            registration_generation: 2,
            worker_incarnation: 3,
            sequence: 0,
        }, canary);
        const bytes = new Uint8Array(frame);
        const metadataLength = new DataView(frame).getUint32(8, true);
        const metadata = new TextDecoder().decode(bytes.subarray(16, 16 + metadataLength));
        expect(metadata).not.toContain('sensitive');
        expect(metadata).not.toContain(String.fromCharCode(...canary));

        const decoded = execPtyWireForTest.decodeFrame(frame);
        expect(decoded?.kind).toBe(1);
        expect([...decoded!.data]).toEqual([...canary]);
    });

    it('rejects a frame whose declared length does not match its payload', () => {
        const frame = execPtyWireForTest.encodeFrame(2, {
            stream_id: 'stream-1',
            execution_generation: 'generation-1',
        });
        const corrupt = frame.slice(0);
        new DataView(corrupt).setUint32(12, 1, true);
        expect(execPtyWireForTest.decodeFrame(corrupt)).toBeNull();
    });
});
