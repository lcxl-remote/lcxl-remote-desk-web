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

export class StubDataChannel {
    static last: StubDataChannel | null = null;
    binaryType = 'blob';
    readyState = 'open';
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
    localDescription: unknown = null;
    onicecandidate: ((ev: { candidate: unknown }) => void) | null = null;
    oniceconnectionstatechange: (() => void) | null = null;
    iceConnectionState = 'new';
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
    async addIceCandidate() {}
    close() {}
}

export class StubWebSocket {
    static instances: StubWebSocket[] = [];
    static OPEN = 1;
    readyState = 1;
    onopen: (() => void) | null = null;
    onclose: (() => void) | null = null;
    onerror: ((ev: unknown) => void) | null = null;
    onmessage: ((ev: { data: string }) => void) | null = null;
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

export const flush = async () => {
    await act(async () => {
        for (let i = 0; i < 8; i++) await Promise.resolve();
        await new Promise((r) => setTimeout(r, 0));
    });
};

/** Drive the signaling handshake until the data channel is open. */
export async function openChannel(): Promise<StubDataChannel> {
    await flush();
    const ws = StubWebSocket.instances[StubWebSocket.instances.length - 1];
    act(() => ws.onopen?.());
    act(() =>
        ws.onmessage?.({
            data: JSON.stringify({
                signaling_type: 101, // INIT
                signaling_data: { ice_servers: [], desk_settings: {} },
            }),
        }),
    );
    await flush();
    const dc = StubDataChannel.last!;
    act(() => dc.onopen?.());
    await flush();
    return dc;
}

export const parseSent = (text: string) => JSON.parse(text);

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
