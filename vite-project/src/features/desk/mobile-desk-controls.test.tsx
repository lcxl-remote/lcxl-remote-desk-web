import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { MobileDeskControls } from './mobile-desk-controls';
import { MobileInteraction } from './mobile-interaction';
import type { useMobileDeskInteraction } from './use-mobile-desk-interaction';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('mobile desk toolbar visibility', () => {
    let media: MediaQueryList;

    beforeEach(() => {
        media = Object.assign(new EventTarget(), { matches: false, media: '(pointer: coarse)' }) as MediaQueryList;
        vi.stubGlobal('matchMedia', vi.fn(() => media));
    });
    afterEach(() => vi.unstubAllGlobals());

    function open() {
        const model = new MobileInteraction(vi.fn());
        model.updateGeometry(320, 240, 1920, 1080);
        const interaction = {
            model,
            change: (action: (model: MobileInteraction) => void) => action(model),
        } as ReturnType<typeof useMobileDeskInteraction>;
        const onOpenChange = vi.fn();
        return { ...render(<MobileDeskControls interaction={interaction} onOpenChange={onOpenChange} />), model, onOpenChange };
    }

    it('hides the touch and zoom buttons for a mouse controller even in a narrow window', () => {
        vi.stubGlobal('innerWidth', 390);
        const view = open();
        expect(view.container).toBeEmptyDOMElement();
        expect(window.matchMedia).toHaveBeenCalledWith('(pointer: coarse)');
    });

    it('shows all four controls on a touch controller even in a wide tablet viewport', () => {
        vi.stubGlobal('innerWidth', 1024);
        Object.assign(media, { matches: true });
        open();
        for (const name of ['controls', 'zoomIn', 'zoomOut', 'fit']) {
            expect(screen.getByRole('button', { name: `pages.desk.mobile.${name}` })).toBeVisible();
        }
    });

    it('keeps touch zoom and fit actions working', () => {
        Object.assign(media, { matches: true });
        const view = open();
        fireEvent.click(screen.getByRole('button', { name: 'pages.desk.mobile.zoomIn' }));
        expect(view.model.zoom).toBe(1.5);
        fireEvent.click(screen.getByRole('button', { name: 'pages.desk.mobile.fit' }));
        expect(view.model.zoom).toBe(1);
        expect(view.model.pan).toEqual({ x: 0, y: 0 });
    });

    it('restores fit and closes the touch menu when switching to mouse input', () => {
        Object.assign(media, { matches: true });
        const view = open();
        view.model.zoomBy(2);
        fireEvent.click(screen.getByRole('button', { name: 'pages.desk.mobile.controls' }));
        expect(view.onOpenChange).toHaveBeenLastCalledWith(true);
        act(() => {
            Object.assign(media, { matches: false });
            media.dispatchEvent(new Event('change'));
        });
        expect(view.container).toBeEmptyDOMElement();
        expect(view.model.zoom).toBe(1);
        expect(view.onOpenChange).toHaveBeenLastCalledWith(false);
        const remove = vi.spyOn(media, 'removeEventListener');
        view.unmount();
        expect(remove).toHaveBeenCalledWith('change', expect.any(Function));
    });
});
