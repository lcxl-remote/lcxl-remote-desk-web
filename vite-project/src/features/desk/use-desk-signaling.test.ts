import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useDeskSignaling } from "./use-desk-signaling";
import {
    SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
    SIGNALING_TYPE_CODE_HEARTBEAT,
} from "./constants";

/**
 * Minimal stub WebSocket so the hook's open/queue paths are exercised
 * without a real network round-trip. Captures everything that goes
 * over `.send` so the request_id contract can be inspected.
 */
class StubWebSocket {
    static instances: StubWebSocket[] = [];
    readyState = WebSocket.OPEN;
    onopen: ((ev: any) => void) | null = null;
    onclose: ((ev: any) => void) | null = null;
    onerror: ((ev: any) => void) | null = null;
    onmessage: ((ev: any) => void) | null = null;
    sent: string[] = [];
    constructor(_url: string) {
        StubWebSocket.instances.push(this);
        // Defer the open callback so React effects can attach handlers.
        setTimeout(() => {
            this.onopen?.({} as any);
        }, 0);
    }
    send(payload: string) {
        this.sent.push(payload);
    }
    close() {
        this.readyState = WebSocket.CLOSED;
        this.onclose?.({} as any);
    }
}

describe("useDeskSignaling.sendMessage", () => {
    let originalWebSocket: typeof WebSocket;

    beforeEach(() => {
        originalWebSocket = (globalThis as any).WebSocket;
        (globalThis as any).WebSocket = StubWebSocket;
        StubWebSocket.instances = [];
    });

    afterEach(() => {
        (globalThis as any).WebSocket = originalWebSocket;
    });

    /**
     * Without a requestId argument the hook must mint a fresh UUID for
     * the wire message and return that same id. Callers can then use
     * the returned id to correlate the eventual response without
     * having to peek inside the JSON.
     */
    it("returns the generated request_id when none is provided", async () => {
        const { result } = renderHook(() => useDeskSignaling());
        // Wait for the open callback to flush queued messages.
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });
        let returnedId: string = "";
        act(() => {
            returnedId = result.current.sendMessage(
                SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
                { width: 1920, height: 1080, refresh_hz: 60, auto: true },
                "desk-A",
            );
        });
        expect(returnedId).toMatch(/^[0-9a-f-]{36}$/);
        const ws = StubWebSocket.instances[0];
        expect(ws.sent.length).toBeGreaterThan(0);
        const wire = JSON.parse(ws.sent[ws.sent.length - 1]);
        expect(wire.request_id).toBe(returnedId);
        expect(wire.signaling_type).toBe(
            SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
        );
    });

    /**
     * When the caller passes an explicit `requestId` the hook must
     * forward it verbatim to the wire AND echo it back. The adaptive
     * resolution hook relies on this to drop its own echoed responses
     * via a pending-id set.
     */
    it("returns the provided request_id and uses it on the wire", async () => {
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });
        const explicit = "fixed-uuid-deadbeef";
        let returnedId: string = "";
        act(() => {
            returnedId = result.current.sendMessage(
                SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS,
                { width: 1280, height: 720, refresh_hz: 0, auto: true },
                "desk-B",
                explicit,
            );
        });
        expect(returnedId).toBe(explicit);
        const ws = StubWebSocket.instances[0];
        const wire = JSON.parse(ws.sent[ws.sent.length - 1]);
        expect(wire.request_id).toBe(explicit);
    });
});

/**
 * Controllable stub: starts in CONNECTING and only opens when the test
 * calls `triggerOpen()`, so the offline-queue paths of `sendTracked` /
 * `cancelQueued` are exercised deterministically. Defines the real
 * readyState constants (the module compares against `WebSocket.OPEN`).
 */
class ControllableWebSocket {
    static instances: ControllableWebSocket[] = [];
    static autoOpen = false;
    static failSendOnce = false;
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    readyState: number = ControllableWebSocket.CONNECTING;
    onopen: ((ev: any) => void) | null = null;
    onclose: ((ev: any) => void) | null = null;
    onerror: ((ev: any) => void) | null = null;
    onmessage: ((ev: any) => void) | null = null;
    sent: string[] = [];
    constructor(_url: string) {
        ControllableWebSocket.instances.push(this);
        if (ControllableWebSocket.autoOpen) {
            setTimeout(() => this.triggerOpen(), 0);
        }
    }
    triggerOpen() {
        this.readyState = ControllableWebSocket.OPEN;
        this.onopen?.({} as any);
    }
    send(payload: string) {
        if (ControllableWebSocket.failSendOnce) {
            ControllableWebSocket.failSendOnce = false;
            throw new Error("simulated ws.send failure");
        }
        this.sent.push(payload);
    }
    close() {
        this.readyState = ControllableWebSocket.CLOSED;
        this.onclose?.({} as any);
    }
}

const OFFER_TYPE = 1;

describe("useDeskSignaling.sendTracked / cancelQueued", () => {
    let originalWebSocket: typeof WebSocket;

    beforeEach(() => {
        originalWebSocket = (globalThis as any).WebSocket;
        (globalThis as any).WebSocket = ControllableWebSocket;
        ControllableWebSocket.instances = [];
        ControllableWebSocket.autoOpen = false;
        ControllableWebSocket.failSendOnce = false;
    });

    afterEach(() => {
        (globalThis as any).WebSocket = originalWebSocket;
    });

    /**
     * While offline, repeated `sendTracked` with the same `replaceKey`
     * collapse to the newest message; on reconnect-flush only that one
     * reaches the wire and only its `onSent` fires (the superseded item's
     * callback is dropped).
     */
    it("replaceKey keeps only the latest queued message; onSent fires on flush", () => {
        const { result } = renderHook(() => useDeskSignaling());
        const sentCbs: string[] = [];
        let r1!: { requestId: string; disposition: string };
        let r2!: { requestId: string; disposition: string };
        act(() => {
            r1 = result.current.sendTracked({
                type: OFFER_TYPE,
                data: { v: 1 },
                replaceKey: "offer:desk-R",
                onSent: (id) => sentCbs.push(`a:${id}`),
            });
            r2 = result.current.sendTracked({
                type: OFFER_TYPE,
                data: { v: 2 },
                replaceKey: "offer:desk-R",
                onSent: (id) => sentCbs.push(`b:${id}`),
            });
        });
        expect(r1.disposition).toBe("queued");
        expect(r2.disposition).toBe("queued");

        const ws = ControllableWebSocket.instances[0];
        // Nothing on the wire and no onSent while still queued.
        expect(ws.sent.length).toBe(0);
        expect(sentCbs).toEqual([]);

        act(() => {
            ws.triggerOpen();
        });
        expect(ws.sent.length).toBe(1);
        expect(JSON.parse(ws.sent[0]).signaling_data).toEqual({ v: 2 });
        expect(sentCbs).toEqual([`b:${r2.requestId}`]);
    });

    /** `cancelQueued` purges a pending OFFER: it is never sent and its
     *  `onSent` never fires, even after a reconnect-flush. */
    it("cancelQueued drops the message; not sent and onSent not fired on flush", () => {
        const { result } = renderHook(() => useDeskSignaling());
        const sentCbs: string[] = [];
        act(() => {
            result.current.sendTracked({
                type: OFFER_TYPE,
                data: { v: 1 },
                replaceKey: "offer:desk-C",
                onSent: (id) => sentCbs.push(id),
            });
            result.current.cancelQueued("offer:desk-C");
        });
        const ws = ControllableWebSocket.instances[0];
        act(() => {
            ws.triggerOpen();
        });
        expect(ws.sent.length).toBe(0);
        expect(sentCbs).toEqual([]);
    });

    /** When the socket is open, `sendTracked` delivers immediately and
     *  fires `onSent` synchronously with disposition `sent`. */
    it("sends immediately when open and fires onSent synchronously", async () => {
        ControllableWebSocket.autoOpen = true;
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });
        const sentCbs: string[] = [];
        let res!: { requestId: string; disposition: string };
        act(() => {
            res = result.current.sendTracked({
                type: OFFER_TYPE,
                data: { v: 9 },
                replaceKey: "offer:desk-O",
                onSent: (id) => sentCbs.push(id),
            });
        });
        expect(res.disposition).toBe("sent");
        expect(sentCbs).toEqual([res.requestId]);
        const ws = ControllableWebSocket.instances[0];
        expect(JSON.parse(ws.sent[ws.sent.length - 1]).signaling_data).toEqual({
            v: 9,
        });
    });

    /** A throwing `ws.send` must not lose the message or fire `onSent`:
     *  it is re-queued (disposition `queued`) and delivered on the next
     *  flush. */
    it("retains the message and defers onSent when ws.send throws", async () => {
        ControllableWebSocket.autoOpen = true;
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });
        const ws = ControllableWebSocket.instances[0];
        const sentCbs: string[] = [];
        ControllableWebSocket.failSendOnce = true;
        let res!: { requestId: string; disposition: string };
        act(() => {
            res = result.current.sendTracked({
                type: OFFER_TYPE,
                data: { v: 7 },
                replaceKey: "offer:desk-F",
                onSent: (id) => sentCbs.push(id),
            });
        });
        expect(res.disposition).toBe("queued");
        expect(sentCbs).toEqual([]);

        // A subsequent flush re-sends the retained message.
        act(() => {
            ws.triggerOpen();
        });
        expect(sentCbs).toEqual([res.requestId]);
    });
});

describe("useDeskSignaling.subscribe (lossless delivery)", () => {
    let originalWebSocket: typeof WebSocket;

    beforeEach(() => {
        originalWebSocket = (globalThis as any).WebSocket;
        (globalThis as any).WebSocket = StubWebSocket;
        StubWebSocket.instances = [];
    });

    afterEach(() => {
        (globalThis as any).WebSocket = originalWebSocket;
    });

    /**
     * Regression guard for the LAN connection-failure root cause: a burst
     * of messages arriving back-to-back within one tick (the exact shape
     * of trickled ICE candidates) must ALL reach subscribers, in order.
     * The previous single-value `lastMessage` channel coalesced such a
     * burst down to its first and last value, silently dropping the
     * middle — which on a LAN is where the only routable host candidate
     * tends to land.
     */
    it("delivers every message of a same-tick burst to subscribers, in order", async () => {
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });

        const received: number[] = [];
        act(() => {
            result.current.subscribe((msg) =>
                received.push(msg.signaling_data.n),
            );
        });

        const ws = StubWebSocket.instances[0];
        act(() => {
            for (let n = 0; n < 5; n += 1) {
                ws.onmessage?.({
                    data: JSON.stringify({
                        signaling_type: 99,
                        signaling_data: { n },
                    }),
                } as any);
            }
        });
        expect(received).toEqual([0, 1, 2, 3, 4]);
    });

    /** Heartbeat responses are consumed internally and never fan out to
     *  subscribers. */
    it("does not deliver heartbeat messages to subscribers", async () => {
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });

        const received: number[] = [];
        act(() => {
            result.current.subscribe((msg) => received.push(msg.signaling_type));
        });

        const ws = StubWebSocket.instances[0];
        act(() => {
            ws.onmessage?.({
                data: JSON.stringify({
                    signaling_type: SIGNALING_TYPE_CODE_HEARTBEAT,
                    signaling_data: null,
                }),
            } as any);
            ws.onmessage?.({
                data: JSON.stringify({ signaling_type: 42, signaling_data: null }),
            } as any);
        });
        expect(received).toEqual([42]);
    });

    /** Unsubscribing removes the handler; later messages are not delivered. */
    it("stops delivering to a handler after it unsubscribes", async () => {
        const { result } = renderHook(() => useDeskSignaling());
        await act(async () => {
            await new Promise((r) => setTimeout(r, 1));
        });

        const received: number[] = [];
        let unsubscribe: () => void = () => {};
        act(() => {
            unsubscribe = result.current.subscribe((msg) =>
                received.push(msg.signaling_data.n),
            );
        });

        const ws = StubWebSocket.instances[0];
        act(() => {
            ws.onmessage?.({
                data: JSON.stringify({ signaling_type: 99, signaling_data: { n: 1 } }),
            } as any);
        });
        act(() => {
            unsubscribe();
        });
        act(() => {
            ws.onmessage?.({
                data: JSON.stringify({ signaling_type: 99, signaling_data: { n: 2 } }),
            } as any);
        });
        expect(received).toEqual([1]);
    });
});
