import { describe, it, expect } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useBeforeUnloadConfirm } from './use-before-unload-confirm';

function fireBeforeUnload(): Event {
    const event = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(event);
    return event;
}

describe('useBeforeUnloadConfirm', () => {
    it('prevents unload (triggers the confirm dialog) while enabled', () => {
        renderHook(() => useBeforeUnloadConfirm(true));
        expect(fireBeforeUnload().defaultPrevented).toBe(true);
    });

    it('does not interfere when disabled', () => {
        renderHook(() => useBeforeUnloadConfirm(false));
        expect(fireBeforeUnload().defaultPrevented).toBe(false);
    });

    it('removes the listener on unmount', () => {
        const { unmount } = renderHook(() => useBeforeUnloadConfirm(true));
        unmount();
        expect(fireBeforeUnload().defaultPrevented).toBe(false);
    });

    it('stops prompting once it becomes disabled', () => {
        const { rerender } = renderHook(({ enabled }) => useBeforeUnloadConfirm(enabled), {
            initialProps: { enabled: true },
        });
        expect(fireBeforeUnload().defaultPrevented).toBe(true);
        rerender({ enabled: false });
        expect(fireBeforeUnload().defaultPrevented).toBe(false);
    });
});
