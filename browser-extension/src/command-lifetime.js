// A timed-out wait must also revoke permission for late continuations to submit.
export async function runBoundedCommand(run, isCurrent, onTimeout, timeoutMs = 32000) {
    const deadline = performance.now() + timeoutMs;
    let active = true;
    let timer;
    const guard = () => {
        if (!active || performance.now() >= deadline || !isCurrent()) {
            throw new Error("bridge_command_expired");
        }
    };
    try {
        const expired = new Promise((_, reject) => {
            timer = setTimeout(() => {
                active = false;
                try { onTimeout(); } catch { /* Revocation remains authoritative if closing fails. */ }
                reject(new Error("bridge_command_timeout"));
            }, timeoutMs);
        });
        return await Promise.race([
            Promise.resolve().then(() => { guard(); return run(guard); }),
            expired,
        ]);
    } finally {
        active = false;
        clearTimeout(timer);
    }
}
