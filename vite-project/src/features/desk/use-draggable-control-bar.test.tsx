import { useRef } from 'react';
import { act, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useDraggableControlBar } from './use-draggable-control-bar';
import { TOOLBAR_STORAGE_KEY } from './toolbar-layout';
const capture = vi.fn(), release = vi.fn();
let resize = () => {};
let stageWidth = 320, barWidth = 200;
function Fixture({ persistent = true }: { persistent?: boolean }) {
    const wrapperRef = useRef<HTMLDivElement>(null), controlBarRef = useRef<HTMLDivElement>(null);
    const toolbar = useDraggableControlBar({ wrapperRef, controlBarRef, persistent });
    return <div ref={wrapperRef} data-testid="wrapper" data-edge={toolbar.edge}>
        <div data-testid="video" style={toolbar.viewportStyle} />
        <div ref={controlBarRef} data-testid="bar" style={toolbar.controlBarStyle}>
            <button onPointerDown={toolbar.handleDragStart} data-testid="handle">{String(toolbar.isDragging)}</button>
        </div>
    </div>;
}
beforeEach(() => {
    localStorage.clear();
    stageWidth = 320; barWidth = 200;
    vi.stubGlobal('ResizeObserver', class { constructor(fn: () => void) { resize = fn; } observe() {} disconnect() {} });
    vi.stubGlobal('PointerEvent', class extends MouseEvent {
        pointerId: number; pointerType: string;
        constructor(type: string, init: PointerEventInit) { super(type, init); this.pointerId = init.pointerId ?? 1; this.pointerType = init.pointerType ?? 'mouse'; }
    });
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function () {
        const width = this.dataset.testid === 'bar' ? barWidth : stageWidth, height = this.dataset.testid === 'bar' ? 50 : 240;
        const translation = this.style.transform.match(/^translate\(([-\d.]+)%, ([-\d.]+)%\)$/);
        const x = this.dataset.testid === 'bar' ? (parseFloat(this.style.left) || 0) + Number(translation?.[1] ?? 0) / 100 * width : 0;
        const y = this.dataset.testid === 'bar' ? (parseFloat(this.style.top) || 0) + Number(translation?.[2] ?? 0) / 100 * height : 0;
        return { left: x, top: y, x, y, right: x + width, bottom: y + height, width, height, toJSON() {} };
    });
    vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockImplementation(() => barWidth);
    vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockReturnValue(50);
    HTMLElement.prototype.setPointerCapture = capture; HTMLElement.prototype.releasePointerCapture = release;
    HTMLElement.prototype.hasPointerCapture = () => true;
    capture.mockClear(); release.mockClear();
});
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });
function start(view: ReturnType<typeof render>) {
    const bar = view.getByTestId('bar').getBoundingClientRect();
    fireEvent.pointerDown(view.getByTestId('handle'), { pointerId: 7, button: 0, clientX: bar.left + 12, clientY: bar.top + 12 });
    return bar;
}
it('previews without resizing, then persists a dock only on successful release', () => {
    const view = render(<Fixture />); start(view);
    fireEvent.pointerMove(document, { pointerId: 7, clientX: 150, clientY: 235 });
    expect(view.getByTestId('handle')).toHaveTextContent('true');
    expect(view.getByTestId('video').style.height).toBe('240px');
    expect(localStorage.getItem(TOOLBAR_STORAGE_KEY)).toBeNull();
    fireEvent.pointerUp(document, { pointerId: 7, clientX: 150, clientY: 235 });
    expect(view.getByTestId('wrapper')).toHaveAttribute('data-edge', 'bottom');
    expect(view.getByTestId('video').style.height).toBe('190px');
    expect(JSON.parse(localStorage.getItem(TOOLBAR_STORAGE_KEY)!).placement.edge).toBe('bottom');
});
it('rolls a cancelled preview back without writing local storage', () => {
    const view = render(<Fixture />); start(view);
    fireEvent.pointerMove(document, { pointerId: 7, clientX: 10, clientY: 100 });
    fireEvent.pointerCancel(document, { pointerId: 7 });
    expect(release).toHaveBeenCalledWith(7);
    expect(view.getByTestId('wrapper')).not.toHaveAttribute('data-edge');
    expect(localStorage.getItem(TOOLBAR_STORAGE_KEY)).toBeNull();
});
it('cancels for a second pointer or changed stage bounds without committing a candidate', () => {
    const view = render(<Fixture />); start(view);
    fireEvent.pointerMove(document, { pointerId: 7, clientX: 10, clientY: 100 });
    fireEvent.pointerDown(document, { pointerId: 8, clientX: 160, clientY: 100 });
    fireEvent.pointerUp(document, { pointerId: 7 }); fireEvent.pointerUp(document, { pointerId: 8 });
    act(() => resize());
    expect(view.getByTestId('handle')).toHaveTextContent('false');
    expect(localStorage.getItem(TOOLBAR_STORAGE_KEY)).toBeNull();
});
it('restores a saved edge before a connection is available', () => {
    localStorage.setItem(TOOLBAR_STORAGE_KEY, JSON.stringify({ version: 1, placement: { kind: 'docked', edge: 'top', along: .4 }, expanded: false }));
    const view = render(<Fixture />);
    expect(view.getByTestId('wrapper')).toHaveAttribute('data-edge', 'top');
    expect(view.getByTestId('video').style.top).toBe('50px');
});
it('keeps the horizontal rail thickness when its main axis is shorter than the handle', () => {
    stageWidth = 30; barWidth = 30;
    localStorage.setItem(TOOLBAR_STORAGE_KEY, JSON.stringify({ version: 1, placement: { kind: 'docked', edge: 'top', along: .4 }, expanded: false }));
    const view = render(<Fixture />);
    expect(view.getByTestId('video').style.top).toBe('50px');
    expect(view.getByTestId('video').style.height).toBe('190px');
});
it.each([0, .5, 1])('positions every expansion frame at anchor %s before resize notification', x => {
    localStorage.setItem(TOOLBAR_STORAGE_KEY, JSON.stringify({ version: 1, placement: { kind: 'floating', x, y: 1 }, expanded: false }));
    barWidth = 60;
    const view = render(<Fixture />);
    const bar = view.getByTestId('bar');
    const saved = localStorage.getItem(TOOLBAR_STORAGE_KEY);
    const collapsed = bar.getBoundingClientRect();
    for (const width of [100, 180, 280, 180, 60]) {
        barWidth = width;
        const beforeObserver = bar.getBoundingClientRect();
        expect(beforeObserver.left).toBeCloseTo(16 + x * (288 - width));
        expect(beforeObserver.bottom).toBe(collapsed.bottom);
        expect(beforeObserver.left).toBeGreaterThanOrEqual(16);
        expect(beforeObserver.right).toBeLessThanOrEqual(304);
        act(() => resize());
        expect(bar.getBoundingClientRect()).toMatchObject({
            left: beforeObserver.left, top: beforeObserver.top,
            width: beforeObserver.width, height: beforeObserver.height,
        });
    }
    expect(localStorage.getItem(TOOLBAR_STORAGE_KEY)).toBe(saved);
});

it('keeps a finite session toolbar in memory without reading or changing owner preferences', () => {
    const existing = JSON.stringify({ version: 1, placement: { mode: 'docked', edge: 'bottom' }, expanded: true });
    localStorage.setItem(TOOLBAR_STORAGE_KEY, existing);
    const view = render(<Fixture persistent={false} />);
    expect(view.getByTestId('wrapper')).not.toHaveAttribute('data-edge');
    start(view);
    fireEvent.pointerMove(document, { pointerId: 7, clientX: 150, clientY: 235 });
    fireEvent.pointerUp(document, { pointerId: 7, clientX: 150, clientY: 235 });
    expect(view.getByTestId('wrapper')).toHaveAttribute('data-edge', 'bottom');
    expect(localStorage.getItem(TOOLBAR_STORAGE_KEY)).toBe(existing);
});
