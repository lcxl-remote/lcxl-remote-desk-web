import { v4 as uuidv4 } from 'uuid';
import { deskErrorCodeEnum } from '@/services/types';
import { buildBinaryChunk, FILE_TRANSFER_CHUNK_SIZE, type FileTransferMessage } from './file-transfer-protocol';

// The host's answer can omit max-message-size; RFC 8841 then limits messages to 64 KiB.
export const UPLOAD_CHUNK_SIZE = Math.min(FILE_TRANSFER_CHUNK_SIZE, 64 * 1024 - 1024 - 40);

export type UploadStatus = 'connecting' | 'awaiting_approval' | 'transferring' | 'confirming' | 'completed' | 'error' | 'unconfirmed' | 'cancelled';
export interface UploadProgress {
    transferId: string;
    fileName: string;
    fileSize: number;
    direction: 'upload';
    status: UploadStatus;
    progress: number;
    transferredBytes: number;
    speed: number;
    remainingSeconds: number;
    resolvedTargetDir?: string;
    resolvedFileName?: string;
    errorMessage?: string;
    errorCode?: number;
}
export type UploadTarget = { target_dir: string } | { target_location: 'downloads' };

interface PendingUpload {
    row: UploadProgress;
    accepted: boolean;
    accept: () => void;
    reject: (error: Error) => void;
    finish: (row: UploadProgress) => void;
    reader?: ReadableStreamDefaultReader<Uint8Array>;
    timer?: ReturnType<typeof setTimeout>;
}

export function initialUpload(file: File, transferId: string): UploadProgress {
    return { transferId, fileName: file.name, fileSize: file.size, direction: 'upload', status: 'connecting', progress: 0, transferredBytes: 0, speed: 0, remainingSeconds: 0 };
}

/** Uploads belong to one channel incarnation; this class never owns the peer connection. */
export class UploadController {
    private pending = new Map<string, PendingUpload>();
    private disposed = false;
    readonly channel: RTCDataChannel;
    private changed: (row: UploadProgress) => void;
    private timeoutMs: number;

    constructor(channel: RTCDataChannel, changed: (row: UploadProgress) => void, timeoutMs = 30_000) {
        this.channel = channel;
        this.changed = changed;
        this.timeoutMs = timeoutMs;
        channel.addEventListener('message', this.onMessage);
        channel.addEventListener('close', this.onClose);
        channel.addEventListener('error', this.onClose);
    }

    owns(id: string) { return this.pending.has(id); }

    private update(entry: PendingUpload, patch: Partial<UploadProgress>) {
        Object.assign(entry.row, patch);
        this.changed({ ...entry.row });
    }

    private arm(entry: PendingUpload) {
        if (!this.pending.has(entry.row.transferId)) return;
        clearTimeout(entry.timer);
        entry.timer = setTimeout(() => this.end(entry, entry.row.status === 'confirming' ? 'unconfirmed' : 'error', 'Upload stalled', deskErrorCodeEnum.TIMEOUT), this.timeoutMs);
    }

    private sendCancel(id: string) {
        if (this.channel.readyState === 'open') {
            try { this.channel.send(JSON.stringify({ type: 'transfer_cancel', transfer_id: id })); } catch { /* The channel may close between the check and send. */ }
        }
    }

    private end(entry: PendingUpload, status: UploadStatus, errorMessage?: string, errorCode?: number) {
        if (!this.pending.delete(entry.row.transferId)) return;
        clearTimeout(entry.timer);
        if (status !== 'completed' && entry.row.status !== 'confirming') this.sendCancel(entry.row.transferId);
        void Promise.resolve(entry.reader?.cancel()).catch(() => {});
        entry.reject(new Error(errorMessage ?? 'Transfer ended'));
        this.update(entry, { status, errorMessage, errorCode, ...(status === 'completed' ? { progress: 100 } : {}) });
        entry.finish({ ...entry.row });
    }

    cancel(id: string): boolean {
        const entry = this.pending.get(id);
        if (!entry) return false;
        if (entry.row.status !== 'confirming') this.end(entry, 'cancelled', 'Cancelled');
        return true;
    }

    private onMessage = (event: MessageEvent) => {
        if (typeof event.data !== 'string') return;
        let message: FileTransferMessage;
        try { message = JSON.parse(event.data); } catch { return; }
        const entry = this.pending.get(message.transfer_id);
        if (!entry) return;
        switch (message.type) {
            case 'upload_state_changed':
                if (entry.accepted) return;
                if (message.phase !== 'awaiting_approval' && message.phase !== 'preparing') return;
                this.update(entry, { status: message.phase === 'awaiting_approval' ? 'awaiting_approval' : 'connecting' });
                this.arm(entry);
                break;
            case 'upload_response':
                if (entry.accepted) return;
                if (!message.accepted) { this.end(entry, 'error', message.message ?? 'Upload rejected'); return; }
                entry.accepted = true;
                this.update(entry, { status: 'transferring', resolvedTargetDir: message.resolved_target_dir, resolvedFileName: message.resolved_file_name });
                this.arm(entry);
                entry.accept();
                break;
            case 'transfer_complete':
                if (entry.row.status !== 'confirming') return;
                this.update(entry, { resolvedFileName: message.resolved_file_name ?? entry.row.resolvedFileName, resolvedTargetDir: message.resolved_target_dir ?? entry.row.resolvedTargetDir });
                this.end(entry, 'completed');
                break;
            case 'transfer_error':
                this.end(entry, message.error_code === deskErrorCodeEnum.FILE_SAVE_UNCONFIRMED ? 'unconfirmed' : 'error', message.message, message.error_code);
                break;
        }
    };

    private onClose = () => {
        for (const entry of [...this.pending.values()]) this.end(entry, entry.row.status === 'confirming' ? 'unconfirmed' : 'error', 'File transfer channel closed');
    };

    dispose() {
        if (this.disposed) return;
        this.disposed = true;
        this.channel.removeEventListener('message', this.onMessage);
        this.channel.removeEventListener('close', this.onClose);
        this.channel.removeEventListener('error', this.onClose);
        this.onClose();
    }

    async upload(file: File, target: UploadTarget, id = uuidv4()): Promise<UploadProgress> {
        if (this.disposed || this.channel.readyState !== 'open') throw new Error('File transfer channel is not ready');
        if (this.pending.has(id)) throw new Error('Upload id is already active');
        let accept!: () => void;
        let reject!: (error: Error) => void;
        const accepted = new Promise<void>((resolve, fail) => { accept = resolve; reject = fail; });
        void accepted.catch(() => {});
        let finish!: (row: UploadProgress) => void;
        const result = new Promise<UploadProgress>(resolve => { finish = resolve; });
        const entry: PendingUpload = { row: initialUpload(file, id), accepted: false, accept, reject, finish };
        this.pending.set(id, entry);
        this.changed({ ...entry.row });
        this.arm(entry);
        // The acceptance promise is installed before send, including synchronous fake transports.
        try {
            this.channel.send(JSON.stringify({ type: 'upload_request', transfer_id: id, ...target, file_name: file.name, file_size: file.size, chunk_size: UPLOAD_CHUNK_SIZE, total_chunks: Math.ceil(file.size / UPLOAD_CHUNK_SIZE) }));
            await accepted;
            if (!this.pending.has(id)) return result;
            const reader = file.stream().getReader();
            entry.reader = reader;
            let pending = new Uint8Array(0);
            let index = 0;
            let sent = 0;
            const started = Date.now();
            let lastUi = 0;
            while (this.pending.has(id)) {
                const { done, value } = await reader.read();
                if (!this.pending.has(id)) break;
                if (value) {
                    const joined = new Uint8Array(pending.length + value.length);
                    joined.set(pending); joined.set(value, pending.length); pending = joined;
                }
                while (pending.length >= UPLOAD_CHUNK_SIZE || (done && pending.length > 0)) {
                    if (!this.pending.has(id) || this.channel.readyState !== 'open') throw new Error('Upload interrupted');
                    const size = Math.min(pending.length, UPLOAD_CHUNK_SIZE);
                    const last = sent + size === file.size;
                    if (last) this.update(entry, { status: 'confirming' });
                    this.channel.send(buildBinaryChunk(id, index++, pending.subarray(0, size)));
                    pending = pending.slice(size);
                    sent += size;
                    this.arm(entry);
                    if (Date.now() - lastUi >= 300 || last) {
                        const speed = sent / Math.max((Date.now() - started) / 1000, 0.001);
                        if (this.pending.has(id)) this.update(entry, { transferredBytes: sent, progress: file.size ? Math.round(sent / file.size * 100) : 100, speed, remainingSeconds: (file.size - sent) / speed });
                        lastUi = Date.now();
                    }
                    let buffered = this.channel.bufferedAmount;
                    let progressedAt = Date.now();
                    while (this.pending.has(id) && this.channel.bufferedAmount > 512 * 1024) {
                        if (this.channel.readyState !== 'open') throw new Error('File transfer channel closed');
                        await new Promise(resolve => setTimeout(resolve, 50));
                        if (this.channel.bufferedAmount < buffered) { progressedAt = Date.now(); this.arm(entry); }
                        buffered = this.channel.bufferedAmount;
                        if (Date.now() - progressedAt >= this.timeoutMs) throw new Error('Upload stalled');
                    }
                }
                if (done) break;
            }
            if (this.pending.has(id)) {
                this.update(entry, { status: 'confirming', transferredBytes: sent });
                this.arm(entry);
                this.channel.send(JSON.stringify({ type: 'transfer_complete', transfer_id: id }));
            }
        } catch (error) {
            this.end(entry, entry.row.status === 'confirming' ? 'unconfirmed' : 'error', error instanceof Error ? error.message : 'Upload failed');
        } finally {
            void Promise.resolve(entry.reader?.cancel()).catch(() => {});
            entry.reader = undefined;
        }
        return result;
    }
}
