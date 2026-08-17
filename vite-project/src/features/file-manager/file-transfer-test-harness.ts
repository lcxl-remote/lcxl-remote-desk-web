/**
 * Shared stubs for hook-level `useFileTransfer` tests.
 *
 * NOTE: with this React 19 + @testing-library/react combination, a hook
 * test that drives the async signaling handshake leaves the act
 * environment in a state where a *subsequent* `renderHook` in the same
 * file does not commit. The practical consequence is **one async
 * `renderHook` test per file** — so each file exercises a single
 * comprehensive scenario across several phases on one connection.
 */
import { act } from '@testing-library/react';
import { vi } from 'vitest';

export class StubDataChannel {
    static last: StubDataChannel | null = null;
    binaryType = 'blob';
    // A freshly created channel is not open yet — `openChannel` is what opens it.
    // Starting here is what lets a test model a channel that never opens at all.
    readyState = 'connecting';
    bufferedAmount = 0;
    sent: Array<string | ArrayBuffer> = [];
    onopen: (() => void) | null = null;
    onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
    onerror: ((ev: unknown) => void) | null = null;
    constructor() {
        StubDataChannel.last = this;
    }
    send(payload: string | ArrayBuffer) {
        this.sent.push(payload);
    }
    close() {
        this.readyState = 'closed';
    }
    get textSent(): string[] {
        return this.sent.filter((s): s is string => typeof s === 'string');
    }
    get binarySent(): ArrayBuffer[] {
        return this.sent.filter((s): s is ArrayBuffer => typeof s !== 'string');
    }
}

export class StubPeerConnection {
    static instances: StubPeerConnection[] = [];
    static get last(): StubPeerConnection | null {
        return StubPeerConnection.instances[StubPeerConnection.instances.length - 1] ?? null;
    }
    localDescription: unknown = null;
    onicecandidate: ((ev: { candidate: unknown }) => void) | null = null;
    oniceconnectionstatechange: (() => void) | null = null;
    iceConnectionState = 'new';
    // A fresh peer connection is gathering, not complete: the stall scenario —
    // gathering that never finishes — is the default, and a test that wants a
    // finished gathering says so with `emitGatheringComplete`.
    iceGatheringState = 'gathering';
    closed = false;
    addedCandidates: unknown[] = [];
    readonly config: { iceServers?: unknown };
    constructor(config?: { iceServers?: unknown }) {
        this.config = config ?? {};
        StubPeerConnection.instances.push(this);
    }
    createDataChannel() {
        return new StubDataChannel() as unknown as RTCDataChannel;
    }
    async createOffer() {
        return { type: 'offer', sdp: 'stub' };
    }
    async setLocalDescription(desc: unknown) {
        this.localDescription = desc;
    }
    async setRemoteDescription() {}
    async addIceCandidate(candidate: unknown) {
        this.addedCandidates.push(candidate);
    }
    close() {
        this.closed = true;
    }
    /** Emit one locally gathered candidate, as a candidate line. */
    emitCandidate(sdp: string) {
        this.onicecandidate?.({
            candidate: { candidate: sdp, toJSON: () => ({ candidate: sdp }) },
        });
    }
    /** Emit end-of-candidates. */
    emitGatheringComplete() {
        this.iceGatheringState = 'complete';
        this.onicecandidate?.({ candidate: null });
    }
    /** Drive an ICE connection state transition. */
    setIceConnectionState(state: string) {
        this.iceConnectionState = state;
        this.oniceconnectionstatechange?.();
    }
}

export class StubWebSocket {
    static instances: StubWebSocket[] = [];
    static OPEN = 1;
    readyState = 1;
    onopen: (() => void) | null = null;
    onclose: (() => void) | null = null;
    onerror: ((ev: unknown) => void) | null = null;
    // The hook's handler is async. Typed as such so callers can await it: an
    // async handler invoked inside a synchronous `act` returns a promise nobody
    // awaits, which mixes act scopes and silently stops React from committing
    // anything afterwards.
    onmessage: ((ev: { data: string }) => void | Promise<void>) | null = null;
    sent: string[] = [];
    constructor(_url: string) {
        StubWebSocket.instances.push(this);
    }
    send(payload: string) {
        this.sent.push(payload);
    }
    close() {
        this.readyState = 3;
    }
}

export interface SignalingGlobals {
    origWS: unknown;
    origPC: unknown;
}

export function installSignalingStubs(): SignalingGlobals {
    StubWebSocket.instances = [];
    StubPeerConnection.instances = [];
    StubDataChannel.last = null;
    const origWS = (globalThis as any).WebSocket;
    const origPC = (globalThis as any).RTCPeerConnection;
    (globalThis as any).WebSocket = StubWebSocket;
    (globalThis as any).RTCPeerConnection = StubPeerConnection;
    return { origWS, origPC };
}

export function restoreSignalingStubs(saved: SignalingGlobals) {
    (globalThis as any).WebSocket = saved.origWS;
    (globalThis as any).RTCPeerConnection = saved.origPC;
}

/**
 * Settle the microtask queue and one macrotask turn.
 *
 * Timeout-driven scenarios have to install fake timers *before* the code under
 * test schedules anything, or the timer they mean to fire was already armed
 * against the real clock. So this drains the pending turn through whichever
 * clock is currently installed rather than assuming the real one.
 */
export const flush = async () => {
    await act(async () => {
        for (let i = 0; i < 8; i++) await Promise.resolve();
        if (vi.isFakeTimers()) {
            await vi.advanceTimersByTimeAsync(0);
        } else {
            await new Promise((r) => setTimeout(r, 0));
        }
    });
};

/** Advance the (fake) clock and settle everything it releases. */
export const advanceTime = async (ms: number) => {
    await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
    });
};

/** The socket the hook is currently using. */
export function latestSocket(): StubWebSocket {
    return StubWebSocket.instances[StubWebSocket.instances.length - 1];
}

/**
 * Deliver one host→browser signaling frame on the current socket.
 *
 * The hook's message handler is async, so the act scope has to await it. Firing
 * it from a synchronous `act` leaves a promise nobody awaits, which mixes act
 * scopes — after that React stops committing and every later state assertion
 * silently reads a stale render.
 */
export async function deliverSignaling(frame: Record<string, unknown>) {
    const ws = latestSocket();
    await act(async () => {
        await ws.onmessage?.({ data: JSON.stringify(frame) });
    });
}

export interface SessionOptions {
    /** ICE servers the host's initialization frame advertises. */
    iceServers?: unknown[];
}

/**
 * Drive the signaling handshake to a live session, stopping short of the data
 * channel.
 *
 * Listing, deletion and host queries need nothing more than this, so a test for
 * them must be able to reach exactly this state and no further.
 */
export async function openSession(options: SessionOptions = {}): Promise<StubWebSocket> {
    await flush();
    const ws = latestSocket();
    act(() => ws.onopen?.());
    await deliverSignaling({
        signaling_type: 101, // REMOTE_ACCESS_INITIALIZED
        signaling_data: {
            ice_servers: options.iceServers ?? [],
            connection_epoch: "test-epoch",
        },
    });
    await flush();
    return ws;
}

/** Drive the signaling handshake until the data channel is open. */
export async function openChannel(options: SessionOptions = {}): Promise<StubDataChannel> {
    await openSession(options);
    const dc = StubDataChannel.last!;
    act(() => {
        dc.readyState = 'open';
        dc.onopen?.();
    });
    await flush();
    return dc;
}

export const parseSent = (text: string) => JSON.parse(text);

/** The signaling frames the browser wrote to `ws`, parsed. */
export function sentSignaling(ws: StubWebSocket): any[] {
    return ws.sent.map(parseSent);
}

/** The frames of one signaling type the browser wrote to `ws`. */
export function sentSignalingOfType(ws: StubWebSocket, signalingType: number): any[] {
    return sentSignaling(ws).filter((frame) => frame.signaling_type === signalingType);
}

/** Build a host→browser binary download chunk frame. */
export function binaryChunk(transferId: string, index: number, data: Uint8Array): ArrayBuffer {
    const buf = new ArrayBuffer(40 + data.length);
    const view = new DataView(buf);
    new Uint8Array(buf).set(new TextEncoder().encode(transferId), 0);
    view.setUint32(36, index, false);
    new Uint8Array(buf, 40).set(data);
    return buf;
}

/** A File whose stream emits two `>=chunkSize` chunks, with the second
 * read gated on a manually released promise so a control message can be
 * injected between chunks. */
export function makeGatedFile(name: string, chunkA: Uint8Array, chunkB: Uint8Array) {
    let release!: () => void;
    const gate = new Promise<void>((r) => {
        release = r;
    });
    let idx = 0;
    const stream = {
        getReader() {
            return {
                async read() {
                    if (idx === 0) {
                        idx++;
                        return { done: false, value: chunkA };
                    }
                    if (idx === 1) {
                        idx++;
                        await gate;
                        return { done: false, value: chunkB };
                    }
                    return { done: true, value: undefined };
                },
                cancel() {},
            };
        },
    };
    const file = {
        name,
        size: chunkA.length + chunkB.length,
        stream: () => stream,
    } as unknown as File;
    return { file, release: () => release() };
}

export const CHUNK = 240 * 1024;
