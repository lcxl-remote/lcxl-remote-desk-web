import { describe, expect, it, vi } from 'vitest';
import {
    BufferedDownloadSink,
    StreamingDownloadSink,
    type WritableFileStreamLike,
} from './download-sink';

/** Fake writable that records the order of operations and can delay writes. */
class FakeWritable implements WritableFileStreamLike {
    written: Uint8Array[] = [];
    log: string[] = [];
    closed = false;
    aborted = false;
    private writeDelayMs = 0;

    constructor(writeDelayMs = 0) {
        this.writeDelayMs = writeDelayMs;
    }

    async write(data: Uint8Array): Promise<void> {
        if (this.writeDelayMs > 0) {
            await new Promise((r) => setTimeout(r, this.writeDelayMs));
        }
        this.written.push(data);
        this.log.push(`write:${data[0]}`);
    }
    async close(): Promise<void> {
        this.closed = true;
        this.log.push('close');
    }
    async abort(): Promise<void> {
        this.aborted = true;
        this.log.push('abort');
    }
}

const chunk = (marker: number, len = 1) => new Uint8Array(len).fill(marker);

describe('StreamingDownloadSink', () => {
    it('writes in-order chunks sequentially then closes on finalize', async () => {
        const w = new FakeWritable();
        const sink = new StreamingDownloadSink(w);
        await sink.write(0, chunk(10));
        await sink.write(1, chunk(20));
        await sink.finalize();
        expect(w.log).toEqual(['write:10', 'write:20', 'close']);
        expect(w.closed).toBe(true);
    });

    it('reorders out-of-order chunks via the buffer before writing', async () => {
        const w = new FakeWritable();
        const sink = new StreamingDownloadSink(w);
        // Deliver chunk 1 before chunk 0.
        const p1 = sink.write(1, chunk(20));
        const p0 = sink.write(0, chunk(10));
        await Promise.all([p1, p0]);
        await sink.finalize();
        expect(w.log).toEqual(['write:10', 'write:20', 'close']);
    });

    it('does not close before all delayed writes complete', async () => {
        const w = new FakeWritable(20); // each write resolves after 20ms
        const sink = new StreamingDownloadSink(w);
        // Fire writes and finalize without awaiting the writes — the
        // synchronous onmessage burst this simulates must not let close
        // jump ahead of the still-pending writes.
        void sink.write(0, chunk(10));
        void sink.write(1, chunk(20));
        const done = sink.finalize();
        // close must be the very last entry, after both writes.
        await done;
        expect(w.log).toEqual(['write:10', 'write:20', 'close']);
        expect(w.closed).toBe(true);
    });

    it('zero-chunk download finalizes to an empty closed file', async () => {
        const w = new FakeWritable();
        const sink = new StreamingDownloadSink(w);
        await sink.finalize();
        expect(w.written).toHaveLength(0);
        expect(w.closed).toBe(true);
    });

    it('abort calls writable.abort and stops further writes', async () => {
        const w = new FakeWritable();
        const sink = new StreamingDownloadSink(w);
        await sink.write(0, chunk(10));
        await sink.abort();
        await sink.write(1, chunk(20)); // no-op after abort
        expect(w.aborted).toBe(true);
        expect(w.written.map((c) => c[0])).toEqual([10]);
    });

    it('propagates a writable.abort rejection to the caller', async () => {
        const w = new FakeWritable();
        w.abort = vi.fn().mockRejectedValue(new Error('abort failed'));
        const sink = new StreamingDownloadSink(w);
        await expect(sink.abort()).rejects.toThrow('abort failed');
    });
});

describe('BufferedDownloadSink', () => {
    it('assembles chunks in order and invokes the saver', async () => {
        const saver = vi.fn().mockResolvedValue(undefined);
        const sink = new BufferedDownloadSink(2, 'f.bin', saver);
        await sink.write(0, chunk(1, 2));
        await sink.write(1, chunk(2, 3));
        await sink.finalize();
        expect(saver).toHaveBeenCalledTimes(1);
        const [blob, name] = saver.mock.calls[0];
        expect(name).toBe('f.bin');
        expect(blob.size).toBe(5);
    });

    it('zero-chunk download saves an empty blob', async () => {
        const saver = vi.fn().mockResolvedValue(undefined);
        const sink = new BufferedDownloadSink(0, 'empty.bin', saver);
        await sink.finalize();
        const [blob] = saver.mock.calls[0];
        expect(blob.size).toBe(0);
    });

    it('abort prevents the saver from running', async () => {
        const saver = vi.fn().mockResolvedValue(undefined);
        const sink = new BufferedDownloadSink(1, 'f.bin', saver);
        await sink.write(0, chunk(1));
        await sink.abort();
        await sink.finalize();
        expect(saver).not.toHaveBeenCalled();
    });
});
