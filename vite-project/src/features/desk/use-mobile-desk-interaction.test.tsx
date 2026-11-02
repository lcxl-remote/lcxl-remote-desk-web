import { useRef } from 'react';
import { act, fireEvent, render } from '@testing-library/react';
import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { useMobileDeskInteraction } from './use-mobile-desk-interaction';

let interaction: ReturnType<typeof useMobileDeskInteraction>;
let send = vi.fn();
function channel() {
    return Object.assign(new EventTarget(), { readyState: 'open' }) as unknown as RTCDataChannel;
}
const mouse = { current: channel() }, keyboard = { current: channel() }, move = { current: channel() };
const pc = { current: Object.assign(new EventTarget(), { connectionState: 'connected' }) as unknown as RTCPeerConnection };
function Fixture({ available = true, blocked = false, suspended = false }: { available?: boolean; blocked?: boolean; suspended?: boolean }) {
    const videoRef = useRef<HTMLVideoElement>(null);
    interaction = useMobileDeskInteraction({ videoRef, available, blocked, suspended, targetKey: 'display', peerConnection: pc,
        isInputAllowed: () => true, mouseChannel: mouse, keyboardChannel: keyboard, mouseMoveChannel: move });
    interaction.senderRef.current = send;
    return <div data-testid="surface" ref={interaction.surfaceRef}><video ref={videoRef} /></div>;
}
beforeEach(() => {
    vi.useFakeTimers(); send = vi.fn();
    vi.spyOn(performance, 'now').mockImplementation(() => Date.now());
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    Object.defineProperty(HTMLVideoElement.prototype, 'videoWidth', { configurable: true, get: () => 3840 });
    Object.defineProperty(HTMLVideoElement.prototype, 'videoHeight', { configurable: true, get: () => 2160 });
    vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(320);
    vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(240);
});
afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
describe('mobile surface adapter', () => {
    it('keeps viewport manipulation available to viewers and suppresses remote input', () => {
        render(<Fixture available={false} />);
        act(() => interaction.change(m => m.zoomBy(12)));
        expect(interaction.model.zoom).toBe(12);
        act(() => {
            interaction.change(m => m.begin([{ id: 1, x: 160, y: 120 }], 0));
            interaction.change(m => m.end([], [{ id: 1, x: 160, y: 120 }], 50));
        });
        expect(send).not.toHaveBeenCalled();
    });
    it('maps touches on the untransformed surface and cancels timers on channel loss', () => {
        const { getByTestId } = render(<Fixture />);
        const surface = getByTestId('surface');
        const touch = { identifier: 8, clientX: 160, clientY: 120 };
        fireEvent.touchStart(surface, { touches: [touch], changedTouches: [touch] });
        act(() => vi.advanceTimersByTime(500));
        expect(send.mock.calls.filter(([e]) => e.event === 'mousedown')).toHaveLength(1);
        expect(send.mock.calls.find(([e]) => e.event === 'mousedown')?.[0]).toMatchObject({ button: 2, target: { x: .5, y: .5 } });
        fireEvent.touchEnd(surface, { touches: [], changedTouches: [touch] });
        act(() => interaction.change(m => m.begin([{ id: 2, x: 80, y: 120 }], performance.now())));
        Object.defineProperty(mouse.current, 'readyState', { configurable: true, value: 'closed' });
        act(() => mouse.current.dispatchEvent(new Event('close')));
        act(() => vi.advanceTimersByTime(600));
        expect(send.mock.calls.filter(([e]) => e.event === 'mousedown')).toHaveLength(1);
        expect(interaction.model.markers).toHaveLength(0);
        Object.defineProperty(mouse.current, 'readyState', { configurable: true, value: 'open' });
    });
    it('ignores touches that began on a sibling and keeps a single video drag', () => {
        const view = render(<Fixture />);
        const surface = view.getByTestId('surface');
        const a = { identifier: 1, clientX: 160, clientY: 120 };
        const foreign = { identifier: 2, clientX: 10, clientY: 10 };
        fireEvent.touchStart(surface, { touches: [a], changedTouches: [a] });
        const moved = { ...a, clientX: 200 };
        fireEvent.touchMove(surface, { touches: [moved, foreign], changedTouches: [moved] });
        expect(send.mock.calls.filter(([e]) => e.event === 'mousedown')).toHaveLength(1);
        expect(interaction.model.zoom).toBe(1);
    });
    it('consumes interrupted contacts until lift without resetting zoom or control', () => {
        const view = render(<Fixture />);
        const surface = view.getByTestId('surface');
        act(() => interaction.change(m => m.zoomBy(2)));
        const a = { identifier: 3, clientX: 160, clientY: 120 };
        fireEvent.touchStart(surface, { touches: [a], changedTouches: [a] });
        view.rerender(<Fixture suspended />);
        fireEvent.touchEnd(surface, { touches: [], changedTouches: [a] });
        view.rerender(<Fixture />);
        expect(send).not.toHaveBeenCalled();
        expect(interaction.model.zoom).toBe(2);
        expect(interaction.model.canControl).toBe(true);
        fireEvent.touchStart(surface, { touches: [a], changedTouches: [a] });
        fireEvent.touchEnd(surface, { touches: [], changedTouches: [a] });
        expect(send.mock.calls.filter(([e]) => e.event === 'mousedown')).toHaveLength(1);
    });
    it('restores fit before whiteboard interaction', () => {
        const view = render(<Fixture />);
        act(() => interaction.change(m => m.zoomBy(4)));
        view.rerender(<Fixture blocked />);
        expect(interaction.model.zoom).toBe(1);
        expect(interaction.model.canControl).toBe(false);
    });
});
