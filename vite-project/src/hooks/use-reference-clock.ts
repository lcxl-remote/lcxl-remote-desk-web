import { useEffect, useState } from 'react';

// UI expiry is advisory; every submission still needs authoritative validation.
export function useReferenceClock(references: readonly { referenceExpiresAt: string }[]) {
    const [, refresh] = useState(0);
    useEffect(() => {
        let timer: ReturnType<typeof setTimeout> | undefined;
        const schedule = () => {
            clearTimeout(timer);
            const now = Date.now();
            const next = references.reduce((nearest, reference) => {
                const expiry = Date.parse(reference.referenceExpiresAt);
                return expiry > now ? Math.min(nearest, expiry) : nearest;
            }, Infinity);
            // Bound the delay to account for clock changes and browser timer limits.
            if (Number.isFinite(next)) timer = setTimeout(update, Math.min(next - now, 60_000));
        };
        const update = () => { refresh(value => value + 1); schedule(); };
        schedule();
        window.addEventListener('focus', update);
        document.addEventListener('visibilitychange', update);
        return () => {
            clearTimeout(timer);
            window.removeEventListener('focus', update);
            document.removeEventListener('visibilitychange', update);
        };
    }, [references]);
    return Date.now();
}
