import type { TFunction } from 'i18next';

/**
 * Shared plumbing for turning a backend `DeskErrorCode` into something the user
 * can read.
 *
 * The backend never sends localized text: a rejection carries a numeric code
 * plus a raw English `message`, and the control end decides how to present it.
 * Every domain (agent, manager-link, file manager, connection verify) owns a
 * small table from the codes *it* can receive to i18n keys, and shares the
 * lookup and fallback handling here.
 *
 * The fallback is deliberately **not** unified. The three existing callers mean
 * different things by "unknown code":
 *
 * - The agent panel shows the backend `message`, which is often the only detail
 *   available about a model failure.
 * - The manager-link banner shows a localized generic line, because it renders
 *   the backend `message` separately underneath — falling back to the message
 *   would print it twice, in English, as the headline.
 * - Connection verify localizes one specific code and passes everything else
 *   through.
 *
 * So callers pick: {@link deskErrorMessage} falls back to the backend text,
 * {@link deskErrorKeyOr} falls back to a generic key.
 */

/** A domain's table from `DeskErrorCode` value to i18n key. */
export type ErrorCodeKeyMap = Readonly<Record<number, string>>;

/**
 * The `DeskErrorCode` an error carries, or `undefined` when it carries none.
 *
 * A rejected request reaches a `catch` block as `unknown`, and the code rides on
 * the error object itself — `RestResponseError` for REST, `SignalingError` for
 * signaling. Reading it structurally keeps callers from having to know which
 * transport produced the failure.
 */
export function errorCodeOf(error: unknown): number | undefined {
    if (error instanceof Error) {
        const code = (error as { code?: unknown }).code;
        if (typeof code === 'number') return code;
    }
    return undefined;
}

/** The i18n key `code` maps to, or `undefined` when the table has none. */
export function deskErrorKey(
    map: ErrorCodeKeyMap,
    code: number | null | undefined,
): string | undefined {
    return code == null ? undefined : map[code];
}

/**
 * The i18n key for `code`, falling back to `genericKey`.
 *
 * For callers that render a localized line of their own for unknown codes —
 * typically because they display the backend `message` separately.
 */
export function deskErrorKeyOr(
    map: ErrorCodeKeyMap,
    code: number | null | undefined,
    genericKey: string,
): string {
    return deskErrorKey(map, code) ?? genericKey;
}

/**
 * A display message for `code`, falling back to the backend `message` and then
 * to `fallback`.
 *
 * For callers whose only other source of detail is the backend text.
 */
export function deskErrorMessage(
    t: TFunction,
    map: ErrorCodeKeyMap,
    code: number | null | undefined,
    message: string | null | undefined,
    fallback: string,
): string {
    const key = deskErrorKey(map, code);
    if (key) return t(key);
    return message && message.length > 0 ? message : fallback;
}
