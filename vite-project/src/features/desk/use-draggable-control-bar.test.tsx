import { useRef } from 'react';
import { act, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useDraggableControlBar } from './use-draggable-control-bar';

const capture = vi.fn(), release = vi.fn();
function Fixture() {
    const wrapperRef = useRef<HTMLDivElement>(null), controlBarRef = useRef<HTMLDivElement>(null);
    const { handleDragStart, isDragging } = useDraggableControlBar({ wrapperRef, controlBarRef });
    return <div ref={wrapperRef} data-testid="wrapper"><div ref={controlBarRef} data-testid="bar">
        <button onPointerDown={handleDragStart} data-testid="handle">{String(isDragging)}</button>
    </div></div>;
}
beforeEach(() => {
    vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
    vi.stubGlobal('PointerEvent', class extends MouseEvent {
        pointerId: number;
        constructor(type: string, init: PointerEventInit) { super(type, init); this.pointerId = init.pointerId ?? 1; }
    });
    HTMLElement.prototype.setPointerCapture = capture;
    HTMLElement.prototype.releasePointerCapture = release;
    HTMLElement.prototype.hasPointerCapture = () => true;
    capture.mockClear(); release.mockClear();
});
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });
it('captures touch pointers, waits for movement slop and clamps the dock to its viewport', () => {
    const view = render(<Fixture />);
    const wrapper = view.getByTestId('wrapper'), bar = view.getByTestId('bar'), handle = view.getByTestId('handle');
    wrapper.getBoundingClientRect = () => ({ left: 0, top: 0, right: 320, bottom: 240, width: 320, height: 240 }) as DOMRect;
    bar.getBoundingClientRect = () => ({ left: 100, top: 170, width: 200, height: 50 }) as DOMRect;
    Object.defineProperty(bar, 'offsetWidth', { value: 200 });
    Object.defineProperty(bar, 'offsetHeight', { value: 50 });
    fireEvent.pointerDown(handle, { pointerId: 7, button: 0, clientX: 110, clientY: 180 });
    expect(capture).toHaveBeenCalledWith(7);
    fireEvent.pointerMove(document, { pointerId: 7, clientX: 112, clientY: 181 });
    expect(handle.textContent).toBe('false');
    act(() => fireEvent.pointerMove(document, { pointerId: 7, clientX: 500, clientY: 500 }));
    expect(bar.style.left).toBe('120px'); expect(bar.style.top).toBe('190px');
    expect(handle.dataset.wasDragged).toBe('true');
    fireEvent.pointerCancel(document, { pointerId: 7 });
    expect(release).toHaveBeenCalledWith(7); expect(handle.textContent).toBe('false');
});
