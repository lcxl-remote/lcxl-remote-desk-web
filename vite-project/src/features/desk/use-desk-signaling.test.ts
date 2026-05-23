import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useDeskSignaling } from "./use-desk-signaling";
import { SIGNALING_TYPE_CODE_CHANGE_DISPLAY_SETTINGS } from "./constants";

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
        const { result } = renderHook(() => useDeskSignaling("desk-A"));
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
        const { result } = renderHook(() => useDeskSignaling("desk-B"));
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
