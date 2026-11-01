// JSON object member order carries no meaning; array order and text do.
export function sameJsonValue(left: unknown, right: unknown): boolean {
    if (left === right) return true;
    if (Array.isArray(left) || Array.isArray(right)) {
        return Array.isArray(left) && Array.isArray(right) && left.length === right.length
            && left.every((value, index) => sameJsonValue(value, right[index]));
    }
    if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object') return false;
    const a = left as Record<string, unknown>;
    const b = right as Record<string, unknown>;
    const keys = Object.keys(a);
    return keys.length === Object.keys(b).length && keys.every(key => Object.hasOwn(b, key) && sameJsonValue(a[key], b[key]));
}
