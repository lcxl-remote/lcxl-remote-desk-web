const MAGIC = new Uint8Array([0x4c, 0x50, 0x54, 0x59]); // LPTY
const WIRE_VERSION = 1;
const HEADER_BYTES = 16;
const MAX_METADATA_BYTES = 16 * 1024;
const MAX_DATA_BYTES = 64 * 1024;
const MAX_PENDING_OUTPUT_BYTES = 256 * 1024;

const KIND_INPUT = 1;
const KIND_RESIZE = 2;
const KIND_CANCEL = 3;
const KIND_OPENED = 4;
const KIND_OUTPUT = 5;
const KIND_CLOSED = 6;

export type PtyCarrierPhase =
    | 'connecting'
    | 'ready'
    | 'opened'
    | 'closed'
    | 'error';

export type PtyCloseReason =
    | 'exited'
    | 'cancelled'
    | 'timed_out'
    | 'carrier_disconnected'
    | 'sequence_violation'
    | 'slow_consumer'
    | 'session_stale'
    | 'outcome_unknown'
    | 'internal_error';

type PtyBinding = {
    task_id: string;
    execution_generation: string;
    stream_id: string;
    session_target_id: string;
    registration_generation: number;
    worker_incarnation: number;
};

type PtyClosed = PtyBinding & {
    exit_status: number | null;
    reason: PtyCloseReason;
    input_frames: number;
    input_bytes: number;
    output_bytes: number;
};

type DataMetadata = Omit<PtyBinding, 'task_id'> & { sequence: number };

export type ExecPtyClientEvents = {
    onPhase: (phase: PtyCarrierPhase, message?: string) => void;
    onOpened: (binding: Readonly<PtyBinding>) => void;
    onClosed: (closed: Readonly<PtyClosed>) => void;
};

export type ExecPtyPrepare = {
    browserConnectionId: string;
    targetConnectionId: string;
    execRequestId: string;
    deviceId?: string;
};

function websocketUrl(deviceId?: string): string {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = new URL(`${protocol}//${window.location.host}/api/desk/exec-pty`);
    if (deviceId) url.searchParams.set('device_id', deviceId);
    return url.toString();
}

function isSafeCounter(value: unknown): value is number {
    return Number.isSafeInteger(value) && Number(value) >= 0;
}

function boundedId(value: unknown): value is string {
    return typeof value === 'string' && value.length > 0 && value.length <= 128;
}

function parseBinding(value: unknown): PtyBinding | null {
    if (!value || typeof value !== 'object') return null;
    const item = value as Record<string, unknown>;
    if (
        !boundedId(item.task_id)
        || !boundedId(item.execution_generation)
        || !boundedId(item.stream_id)
        || !boundedId(item.session_target_id)
        || !isSafeCounter(item.registration_generation)
        || !isSafeCounter(item.worker_incarnation)
    ) return null;
    return item as PtyBinding;
}

function sameBinding(left: PtyBinding, right: Omit<PtyBinding, 'task_id'>): boolean {
    return left.stream_id === right.stream_id
        && left.execution_generation === right.execution_generation
        && left.session_target_id === right.session_target_id
        && left.registration_generation === right.registration_generation
        && left.worker_incarnation === right.worker_incarnation;
}

function encodeFrame(
    kind: number,
    metadata: object,
    data: Uint8Array<ArrayBufferLike> = new Uint8Array(),
): ArrayBuffer {
    const metadataBytes = new TextEncoder().encode(JSON.stringify(metadata));
    if (metadataBytes.byteLength > MAX_METADATA_BYTES || data.byteLength > MAX_DATA_BYTES) {
        throw new Error('PTY frame exceeds its hard limit');
    }
    const frame = new Uint8Array(HEADER_BYTES + metadataBytes.byteLength + data.byteLength);
    frame.set(MAGIC, 0);
    frame[4] = WIRE_VERSION;
    frame[5] = kind;
    const view = new DataView(frame.buffer);
    view.setUint16(6, 0, true);
    view.setUint32(8, metadataBytes.byteLength, true);
    view.setUint32(12, data.byteLength, true);
    frame.set(metadataBytes, HEADER_BYTES);
    frame.set(data, HEADER_BYTES + metadataBytes.byteLength);
    return frame.buffer;
}

function decodeFrame(buffer: ArrayBuffer): {
    kind: number;
    metadata: unknown;
    data: Uint8Array;
} | null {
    if (buffer.byteLength < HEADER_BYTES) return null;
    const bytes = new Uint8Array(buffer);
    if (!MAGIC.every((value, index) => bytes[index] === value)) return null;
    const view = new DataView(buffer);
    if (bytes[4] !== WIRE_VERSION || view.getUint16(6, true) !== 0) return null;
    const metadataLength = view.getUint32(8, true);
    const dataLength = view.getUint32(12, true);
    if (metadataLength > MAX_METADATA_BYTES || dataLength > MAX_DATA_BYTES) return null;
    if (HEADER_BYTES + metadataLength + dataLength !== buffer.byteLength) return null;
    try {
        const metadata = JSON.parse(
            new TextDecoder('utf-8', { fatal: true }).decode(
                bytes.subarray(HEADER_BYTES, HEADER_BYTES + metadataLength),
            ),
        ) as unknown;
        return {
            kind: bytes[5],
            metadata,
            data: bytes.subarray(HEADER_BYTES + metadataLength),
        };
    } catch {
        return null;
    }
}

/**
 * One execution's non-reconnecting, non-replayable PTY carrier.
 *
 * Input bytes only exist as the argument of `sendInput` and the ArrayBuffer
 * passed directly to `WebSocket.send`; this class never stores or logs them.
 */
export class ExecPtyClient {
    private readonly events: ExecPtyClientEvents;
    private socket: WebSocket | null = null;
    private binding: PtyBinding | null = null;
    private carrierId: string | null = null;
    private nextInputSequence = 0;
    private nextOutputSequence = 0;
    private outputListener: ((data: Uint8Array) => void) | null = null;
    private pendingOutput: Uint8Array[] = [];
    private pendingOutputBytes = 0;
    private disposed = false;

    constructor(events: ExecPtyClientEvents) {
        this.events = events;
    }

    async prepare(request: ExecPtyPrepare): Promise<string> {
        if (this.socket || this.disposed) throw new Error('PTY carrier is not reusable');
        for (const value of [
            request.browserConnectionId,
            request.targetConnectionId,
            request.execRequestId,
        ]) {
            if (!boundedId(value)) throw new Error('PTY carrier binding is invalid');
        }
        this.events.onPhase('connecting');
        return new Promise<string>((resolve, reject) => {
            const socket = new WebSocket(websocketUrl(request.deviceId));
            this.socket = socket;
            socket.binaryType = 'arraybuffer';
            let settled = false;
            const timeout = window.setTimeout(() => {
                if (settled) return;
                settled = true;
                reject(new Error('PTY carrier preparation timed out'));
                this.fail('PTY carrier preparation timed out');
            }, 12_000);

            const settleError = (message: string) => {
                if (!settled) {
                    settled = true;
                    window.clearTimeout(timeout);
                    reject(new Error(message));
                }
                this.fail(message);
            };

            socket.onopen = () => {
                try {
                    socket.send(JSON.stringify({
                        browser_connection_id: request.browserConnectionId,
                        target_connection_id: request.targetConnectionId,
                        exec_request_id: request.execRequestId,
                    }));
                } catch {
                    settleError('PTY carrier preparation failed');
                }
            };
            socket.onmessage = (event) => {
                if (typeof event.data === 'string') {
                    if (settled) {
                        settleError('Unexpected PTY text frame');
                        return;
                    }
                    try {
                        const message = JSON.parse(event.data) as Record<string, unknown>;
                        if (
                            message.type !== 'ready'
                            || !boundedId(message.carrier_id)
                            || message.exec_request_id !== request.execRequestId
                        ) {
                            settleError(
                                typeof message.message === 'string'
                                    ? message.message
                                    : 'PTY carrier was rejected',
                            );
                            return;
                        }
                        settled = true;
                        window.clearTimeout(timeout);
                        this.carrierId = message.carrier_id;
                        this.events.onPhase('ready');
                        resolve(message.carrier_id);
                    } catch {
                        settleError('PTY carrier returned invalid metadata');
                    }
                    return;
                }
                if (!(event.data instanceof ArrayBuffer)) {
                    settleError('PTY carrier returned an unsupported frame');
                    return;
                }
                this.receiveBinary(event.data, request.execRequestId);
            };
            socket.onerror = () => settleError('PTY carrier transport failed');
            socket.onclose = () => {
                if (!this.disposed && this.binding === null) {
                    settleError('PTY carrier closed before execution started');
                } else if (!this.disposed) {
                    this.events.onPhase('closed');
                }
            };
        });
    }

    attachOutput(listener: (data: Uint8Array) => void): () => void {
        this.outputListener = listener;
        for (const chunk of this.pendingOutput) listener(chunk);
        this.pendingOutput = [];
        this.pendingOutputBytes = 0;
        return () => {
            if (this.outputListener === listener) this.outputListener = null;
        };
    }

    sendInput(data: Uint8Array): boolean {
        if (!this.binding || !this.isOpen() || data.byteLength === 0) return false;
        for (let offset = 0; offset < data.byteLength; offset += MAX_DATA_BYTES) {
            const chunk = data.subarray(offset, Math.min(offset + MAX_DATA_BYTES, data.byteLength));
            const sequence = this.takeInputSequence();
            if (sequence === null) return false;
            const metadata: DataMetadata = { ...this.wireBinding(), sequence };
            try {
                this.socket!.send(encodeFrame(KIND_INPUT, metadata, chunk));
            } catch {
                this.fail('PTY input transport failed');
                return false;
            }
        }
        return true;
    }

    resize(rows: number, cols: number): boolean {
        if (!this.binding || !this.isOpen()) return false;
        if (!Number.isInteger(rows) || !Number.isInteger(cols)
            || rows < 1 || rows > 500 || cols < 1 || cols > 500) return false;
        const sequence = this.takeInputSequence();
        if (sequence === null) return false;
        try {
            this.socket!.send(encodeFrame(KIND_RESIZE, {
                ...this.wireBinding(),
                sequence,
                rows,
                cols,
            }));
            return true;
        } catch {
            this.fail('PTY resize transport failed');
            return false;
        }
    }

    cancel(reason: PtyCloseReason = 'cancelled'): void {
        if (this.binding && this.isOpen()) {
            try {
                this.socket!.send(encodeFrame(KIND_CANCEL, {
                    ...this.wireBinding(),
                    reason,
                }));
            } catch {
                // Closing the one-shot socket is itself a fail-closed cancel.
            }
        }
        this.dispose();
    }

    dispose(): void {
        this.disposed = true;
        this.outputListener = null;
        this.pendingOutput = [];
        this.pendingOutputBytes = 0;
        const socket = this.socket;
        this.socket = null;
        if (socket && socket.readyState < WebSocket.CLOSING) socket.close();
    }

    private receiveBinary(buffer: ArrayBuffer, expectedTaskId: string): void {
        const decoded = decodeFrame(buffer);
        if (!decoded) {
            this.fail('PTY carrier returned an invalid frame');
            return;
        }
        if (decoded.kind === KIND_OPENED) {
            const binding = parseBinding(decoded.metadata);
            if (
                decoded.data.byteLength !== 0
                || !binding
                || binding.task_id !== expectedTaskId
                || binding.stream_id !== this.carrierId
                || this.binding
            ) {
                this.fail('PTY execution binding is stale');
                return;
            }
            this.binding = binding;
            this.events.onPhase('opened');
            this.events.onOpened(binding);
            return;
        }
        if (!this.binding || !decoded.metadata || typeof decoded.metadata !== 'object') {
            this.fail('PTY output arrived before the stream opened');
            return;
        }
        if (decoded.kind === KIND_OUTPUT) {
            const metadata = decoded.metadata as Partial<DataMetadata>;
            if (
                decoded.data.byteLength === 0
                || !isSafeCounter(metadata.sequence)
                || metadata.sequence !== this.nextOutputSequence
                || !sameBinding(this.binding, metadata as Omit<PtyBinding, 'task_id'>)
            ) {
                this.fail('PTY output sequence or binding is invalid');
                return;
            }
            this.nextOutputSequence += 1;
            if (this.outputListener) {
                this.outputListener(decoded.data);
            } else if (this.pendingOutputBytes + decoded.data.byteLength <= MAX_PENDING_OUTPUT_BYTES) {
                const copy = decoded.data.slice();
                this.pendingOutput.push(copy);
                this.pendingOutputBytes += copy.byteLength;
            } else {
                this.fail('PTY output consumer is not ready');
            }
            return;
        }
        if (decoded.kind === KIND_CLOSED) {
            const metadata = decoded.metadata as Partial<PtyClosed>;
            if (
                decoded.data.byteLength !== 0
                || !sameBinding(this.binding, metadata as Omit<PtyBinding, 'task_id'>)
                || typeof metadata.reason !== 'string'
            ) {
                this.fail('PTY close binding is invalid');
                return;
            }
            this.events.onClosed({ ...this.binding, ...metadata } as PtyClosed);
            this.events.onPhase('closed');
            this.dispose();
            return;
        }
        this.fail('PTY carrier returned a frame in the wrong direction');
    }

    private wireBinding(): Omit<PtyBinding, 'task_id'> {
        const binding = this.binding!;
        return {
            stream_id: binding.stream_id,
            execution_generation: binding.execution_generation,
            session_target_id: binding.session_target_id,
            registration_generation: binding.registration_generation,
            worker_incarnation: binding.worker_incarnation,
        };
    }

    private takeInputSequence(): number | null {
        if (!Number.isSafeInteger(this.nextInputSequence)) {
            this.fail('PTY input sequence exhausted');
            return null;
        }
        return this.nextInputSequence++;
    }

    private isOpen(): boolean {
        return this.socket?.readyState === WebSocket.OPEN;
    }

    private fail(message: string): void {
        if (this.disposed) return;
        this.events.onPhase('error', message);
        this.dispose();
    }
}

export const execPtyWireForTest = { encodeFrame, decodeFrame };
