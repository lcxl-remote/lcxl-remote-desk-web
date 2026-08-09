import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { useTerminalSessionGuard } from "./use-terminal-session-guard";

function fireBeforeUnload(): Event {
    const event = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(event);
    return event;
}

describe("useTerminalSessionGuard", () => {
    it("prompts only between terminal started and terminal closed", () => {
        const { result } = renderHook(() => useTerminalSessionGuard());

        expect(fireBeforeUnload().defaultPrevented).toBe(false);

        act(() => result.current.markStarted());
        expect(fireBeforeUnload().defaultPrevented).toBe(true);

        act(() => result.current.markClosed());
        expect(fireBeforeUnload().defaultPrevented).toBe(false);
    });

    it("removes the prompt when the terminal view unmounts", () => {
        const { result, unmount } = renderHook(() => useTerminalSessionGuard());

        act(() => result.current.markStarted());
        unmount();

        expect(fireBeforeUnload().defaultPrevented).toBe(false);
    });
});
