import { act, fireEvent, render, screen } from '@testing-library/react';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { ToolbarScrollArea } from './toolbar-scroll-area';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
let resize: () => void;
beforeEach(() => {
    vi.stubGlobal('ResizeObserver', class {
        constructor(callback: () => void) { resize = callback; }
        observe() {}
        disconnect() {}
    });
});
afterEach(() => vi.unstubAllGlobals());
function contents(vertical = false, expanded = true) {
    return <ToolbarScrollArea vertical={vertical} expanded={expanded}>
        {Array.from({ length: 8 }, (_, i) => <button key={i}>Action {i}</button>)}
    </ToolbarScrollArea>;
}
function setup(vertical = false) {
    const view = render(contents(vertical));
    const element = view.container.querySelector<HTMLDivElement>('.controlBarScrollViewport')!;
    Object.defineProperties(element, {
        clientWidth: { configurable: true, value: vertical ? 40 : 120 },
        clientHeight: { configurable: true, value: vertical ? 120 : 40 },
        scrollWidth: { configurable: true, value: vertical ? 40 : 600 },
        scrollHeight: { configurable: true, value: vertical ? 600 : 40 },
    });
    const scrollBy = vi.fn((options: ScrollToOptions) => {
        element.scrollLeft = Math.max(0, Math.min(480, element.scrollLeft + (options.left ?? 0)));
        element.scrollTop = Math.max(0, Math.min(480, element.scrollTop + (options.top ?? 0)));
        fireEvent.scroll(element);
    });
    Object.defineProperty(element, 'scrollBy', { configurable: true, value: scrollBy });
    act(() => resize());
    return { view, element, scrollBy };
}
function wheel(element: HTMLElement, options: WheelEventInit = {}) {
    const event = new WheelEvent('wheel', { deltaY: 100, bubbles: true, cancelable: true, ...options });
    fireEvent(element, event);
    return event;
}
it('maps a mouse wheel onto a horizontal toolbar and shows only available directions', () => {
    const { element } = setup();
    expect(screen.queryByRole('button', { name: 'Scroll toolbar buttons left' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Scroll toolbar buttons right' })).toBeVisible();
    expect(wheel(element).defaultPrevented).toBe(true);
    expect(element.scrollLeft).toBe(100);
    expect(screen.getByRole('button', { name: 'Scroll toolbar buttons left' })).toBeVisible();
    wheel(element, { deltaY: 1000 });
    expect(element.scrollLeft).toBe(480);
    expect(screen.queryByRole('button', { name: 'Scroll toolbar buttons right' })).toBeNull();
    wheel(element, { deltaY: -1000 });
    expect(element.scrollLeft).toBe(0);
    expect(screen.queryByRole('button', { name: 'Scroll toolbar buttons left' })).toBeNull();
});
it('uses the dominant touchpad delta once and preserves browser pinch zoom', () => {
    const { element } = setup();
    wheel(element, { deltaX: 80, deltaY: 10 });
    expect(element.scrollLeft).toBe(80);
    expect(wheel(element, { ctrlKey: true }).defaultPrevented).toBe(false);
    expect(element.scrollLeft).toBe(80);
});
it.each([[1, 2, 32], [2, 1, 120]])('converts wheel mode %s into viewport distance', (deltaMode, deltaY, expected) => {
    const { element } = setup();
    element.style.lineHeight = '16px';
    wheel(element, { deltaMode, deltaY });
    expect(element.scrollLeft).toBe(expected);
});
it('scrolls vertical controls with the wheel and clickable up/down indicators', () => {
    const { element, scrollBy } = setup(true);
    expect(screen.getByRole('button', { name: 'Scroll toolbar buttons down' })).toBeVisible();
    wheel(element);
    expect(element.scrollTop).toBe(100);
    expect(element.scrollLeft).toBe(0);
    fireEvent.click(screen.getByRole('button', { name: 'Scroll toolbar buttons up' }));
    expect(scrollBy).toHaveBeenCalledWith({ top: -90, behavior: 'smooth' });
    expect(element.scrollTop).toBe(10);
});
it('moves horizontal controls by arrow and respects reduced motion', () => {
    const { element, scrollBy } = setup();
    vi.stubGlobal('matchMedia', () => ({ matches: true }));
    fireEvent.click(screen.getByRole('button', { name: 'Scroll toolbar buttons right' }));
    expect(scrollBy).toHaveBeenCalledWith({ left: 90, behavior: 'auto' });
    expect(element.scrollLeft).toBe(90);
});
it('removes indicators when content fits or is collapsed, without trapping the wheel', () => {
    const { view, element } = setup();
    Object.defineProperty(element, 'scrollWidth', { configurable: true, value: 120 });
    act(() => resize());
    expect(screen.queryByRole('button', { name: /Scroll toolbar buttons/ })).toBeNull();
    expect(wheel(element).defaultPrevented).toBe(false);
    Object.defineProperty(element, 'scrollWidth', { configurable: true, value: 600 });
    act(() => resize());
    view.rerender(contents(false, false));
    expect(screen.queryByRole('button', { name: /Scroll toolbar buttons/ })).toBeNull();
    expect(wheel(element).defaultPrevented).toBe(false);
});
it('resets the old scroll axis when docking changes orientation', () => {
    const { view, element } = setup();
    wheel(element);
    Object.defineProperties(element, {
        clientHeight: { configurable: true, value: 120 },
        scrollHeight: { configurable: true, value: 600 },
    });
    view.rerender(contents(true));
    expect(element.scrollLeft).toBe(0);
    expect(element.scrollTop).toBe(0);
    expect(screen.queryByRole('button', { name: 'Scroll toolbar buttons right' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Scroll toolbar buttons down' })).toBeVisible();
});
it.each(['floating', 'top', 'bottom', 'left', 'right'])('keeps hover lift inside the %s toolbar clipping boundary without thickening the rail', edge => {
    const style = document.createElement('style');
    style.textContent = '* { box-sizing: border-box; }\n' + readFileSync(resolve(process.cwd(), 'src/features/desk/desk-session.css'), 'utf8');
    document.head.append(style);
    try {
        const vertical = edge === 'left' || edge === 'right';
        const { container } = render(<div className="controlBar" data-edge={edge}>
            <ToolbarScrollArea expanded vertical={vertical}>
                <button className="controlButton">Action</button>
            </ToolbarScrollArea>
        </div>);
        const bar = getComputedStyle(container.querySelector('.controlBar')!);
        const list = getComputedStyle(container.querySelector('.controlButtons')!);
        const button = getComputedStyle(screen.getByRole('button', { name: 'Action' }));
        const pixels = (value: string) => parseFloat(value) || 0;
        expect(pixels(list.paddingTop)).toBeGreaterThanOrEqual(4);
        expect(pixels(list.paddingLeft)).toBeGreaterThanOrEqual(vertical ? 4 : 6);
        expect(pixels(list.paddingRight)).toBe(pixels(list.paddingLeft));
        const thickness = vertical
            ? pixels(button.width) + pixels(list.paddingLeft) + pixels(list.paddingRight) + pixels(bar.paddingLeft) + pixels(bar.paddingRight) + pixels(bar.borderLeftWidth) + pixels(bar.borderRightWidth)
            : pixels(button.height) + pixels(list.paddingTop) + pixels(list.paddingBottom) + pixels(bar.paddingTop) + pixels(bar.paddingBottom) + pixels(bar.borderTopWidth) + pixels(bar.borderBottomWidth);
        expect(thickness).toBe(54);
    } finally { style.remove(); }
});
