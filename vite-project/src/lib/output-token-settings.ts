export const DEFAULT_MODEL_RUNTIME_OUTPUT_TOKENS = 65536;

/** Parse a positive u32 without accepting blank, fractional or overflow values. */
export function parseOutputTokens(value: string): number | null {
    if (!/^\d+$/.test(value)) return null;
    const parsed = Number(value);
    return Number.isInteger(parsed) && parsed > 0 && parsed <= 4294967295 ? parsed : null;
}
