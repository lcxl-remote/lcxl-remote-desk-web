// Bound retained encrypted text, including the item currently being processed.
export function createInboundQueue(consume, fail, {
    maxItems = 4, maxCharacters = 140 * 1024 * 1024, lifetimeMs = 32000,
    now = () => performance.now(),
} = {}) {
    const pending = [];
    let count = 0;
    let characters = 0;
    let running = false;
    let stopped = false;
    function stop() {
        stopped = true;
        pending.length = 0;
        count = 0;
        characters = 0;
    }
    function reject() {
        stop();
        try { fail(); } catch { /* The queue remains closed even if transport close fails. */ }
    }
    async function drain() {
        running = true;
        try {
            while (!stopped && pending.length) {
                const item = pending.shift();
                if (now() >= item.deadline) { reject(); break; }
                await consume(item.text, item.deadline);
                if (!stopped) { count--; characters -= item.text.length; }
            }
        } catch { reject(); }
        finally { running = false; }
    }
    return {
        stop,
        push(text) {
            if (stopped) return false;
            if (typeof text !== "string" || count >= maxItems || text.length > maxCharacters - characters) {
                reject();
                return false;
            }
            pending.push({ text, deadline: now() + lifetimeMs });
            count++;
            characters += text.length;
            if (!running) void drain();
            return true;
        },
    };
}
