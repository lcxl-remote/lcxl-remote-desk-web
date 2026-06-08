/**
 * Download landing strategies, isolated per `transfer_id`.
 *
 * A download arrives as a stream of binary chunks on the shared
 * `file_transfer_event` data channel. Two concerns drive this module:
 *
 * 1. **Memory.** The previous implementation buffered every chunk in a
 *    `Uint8Array[]` and assembled one giant `Uint8Array` + `Blob` on
 *    completion, so peak memory tracked the whole file size and large
 *    files OOM'd the tab. `StreamingDownloadSink` writes each chunk
 *    straight to a `FileSystemWritableFileStream`, keeping peak memory
 *    at roughly one chunk.
 *
 * 2. **Ordering / concurrency.** The data channel `onmessage` callback
 *    is synchronous and fires chunks back-to-back, while a write to the
 *    underlying stream is async. Without serialization a later chunk's
 *    write — or `finalize()`'s `close()` — could run before an earlier
 *    write settled, corrupting or truncating the file. Every sink keeps
 *    a serial promise tail so writes happen one at a time and
 *    `finalize()` only closes after all queued writes complete. A small
 *    reorder buffer tolerates out-of-order delivery defensively (the
 *    channel is ordered per transfer, but interleaving across transfers
 *    and future protocol changes make this cheap insurance).
 *
 * `BufferedDownloadSink` keeps the legacy in-memory behaviour for
 * browsers without the File System Access API (e.g. Firefox), where a
 * `Blob` + anchor download is the only option.
 */

/** Minimal view of a `FileSystemWritableFileStream` used for streaming writes. */
export interface WritableFileStreamLike {
    write(data: Uint8Array): Promise<void>;
    close(): Promise<void>;
    abort(reason?: unknown): Promise<void>;
}

/** Saves an assembled blob to disk (picker or fallback anchor download). */
export type BlobSaver = (blob: Blob, fileName: string) => Promise<void>;

export interface DownloadSink {
    /** Queue a chunk for writing. Resolves once it (and any earlier
     * in-order chunks it unblocks) have been written. */
    write(chunkIndex: number, data: Uint8Array): Promise<void>;
    /** Flush all queued writes, then finish (close stream / save blob). */
    finalize(): Promise<void>;
    /** Abandon the download and release the underlying resource. */
    abort(): Promise<void>;
}

/**
 * Streams chunks directly to a writable file stream, serialized through
 * a promise tail with a defensive reorder buffer. Real bytes never
 * accumulate in memory.
 */
export class StreamingDownloadSink implements DownloadSink {
    private tail: Promise<void> = Promise.resolve();
    private readonly buffer = new Map<number, Uint8Array>();
    private expectedIndex = 0;
    private aborted = false;
    private readonly writable: WritableFileStreamLike;

    constructor(writable: WritableFileStreamLike) {
        this.writable = writable;
    }

    write(chunkIndex: number, data: Uint8Array): Promise<void> {
        if (this.aborted) return Promise.resolve();
        this.buffer.set(chunkIndex, data);
        this.tail = this.tail.then(() => this.drain());
        return this.tail;
    }

    finalize(): Promise<void> {
        this.tail = this.tail.then(async () => {
            await this.drain();
            if (this.aborted) return;
            await this.writable.close();
        });
        return this.tail;
    }

    async abort(): Promise<void> {
        this.aborted = true;
        this.buffer.clear();
        await this.writable.abort();
    }

    /** Write every contiguous buffered chunk starting at `expectedIndex`. */
    private async drain(): Promise<void> {
        if (this.aborted) return;
        while (this.buffer.has(this.expectedIndex)) {
            const chunk = this.buffer.get(this.expectedIndex)!;
            this.buffer.delete(this.expectedIndex);
            await this.writable.write(chunk);
            if (this.aborted) return;
            this.expectedIndex += 1;
        }
    }
}

/**
 * Buffers chunks in memory and saves the assembled blob on completion.
 * Fallback for browsers without `showSaveFilePicker`. Peak memory
 * tracks the file size — acceptable only where streaming is impossible.
 */
export class BufferedDownloadSink implements DownloadSink {
    private readonly chunks: Uint8Array[];
    private aborted = false;
    private readonly fileName: string;
    private readonly saver: BlobSaver;

    constructor(totalChunks: number, fileName: string, saver: BlobSaver) {
        this.chunks = new Array<Uint8Array>(Math.max(0, totalChunks));
        this.fileName = fileName;
        this.saver = saver;
    }

    write(chunkIndex: number, data: Uint8Array): Promise<void> {
        if (!this.aborted) this.chunks[chunkIndex] = data;
        return Promise.resolve();
    }

    async finalize(): Promise<void> {
        if (this.aborted) return;
        const present = this.chunks.filter((c): c is Uint8Array => !!c);
        const totalSize = present.reduce((sum, c) => sum + c.length, 0);
        const assembled = new Uint8Array(totalSize);
        let offset = 0;
        for (const chunk of present) {
            assembled.set(chunk, offset);
            offset += chunk.length;
        }
        await this.saver(new Blob([assembled]), this.fileName);
    }

    abort(): Promise<void> {
        this.aborted = true;
        this.chunks.length = 0;
        return Promise.resolve();
    }
}
