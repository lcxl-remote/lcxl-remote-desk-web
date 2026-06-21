import { describe, it, expect, afterEach } from 'vitest';
import { fullscreenPortalContainer } from './utils';

function setFullscreenElement(el: Element | null) {
    Object.defineProperty(document, 'fullscreenElement', {
        value: el,
        configurable: true,
    });
}

describe('fullscreenPortalContainer', () => {
    afterEach(() => {
        setFullscreenElement(null);
    });

    it('returns undefined when nothing is fullscreen (Radix falls back to body)', () => {
        setFullscreenElement(null);
        expect(fullscreenPortalContainer()).toBeUndefined();
    });

    it('returns the fullscreen element so overlays render inside the top layer', () => {
        const el = document.createElement('div');
        setFullscreenElement(el);
        expect(fullscreenPortalContainer()).toBe(el);
    });
});
