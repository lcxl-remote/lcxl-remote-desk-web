import { useCallback, useState } from "react";

import { useBeforeUnloadConfirm } from "../desk/use-before-unload-confirm";

export function useTerminalSessionGuard() {
    const [isAlive, setIsAlive] = useState(false);

    useBeforeUnloadConfirm(isAlive);

    const markStarted = useCallback(() => setIsAlive(true), []);
    const markClosed = useCallback(() => setIsAlive(false), []);

    return { isAlive, markStarted, markClosed };
}
