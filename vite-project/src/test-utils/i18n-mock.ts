import enUS from "@/locales/en-US";

const flat = enUS as Record<string, string>;

/**
 * t() backed by the real en-US locale, interpolating {{var}} placeholders from
 * the options object like i18next does. Used by component tests so assertions
 * match the copy users actually see (the app no longer passes inline defaults).
 */
export function tMock(key: string, opts?: Record<string, unknown>): string {
    let s = flat[key] ?? key;
    if (opts && typeof opts === "object") {
        s = s.replace(/\{\{\s*(\w+)\s*\}\}/g, (_m, name) => String(opts[name] ?? ""));
    }
    return s;
}

/** Factory for `vi.mock("react-i18next", () => import(...).then(m => m.reactI18nextMock()))`. */
export function reactI18nextMock() {
    return {
        useTranslation: () => ({
            t: tMock,
            i18n: { language: "en-US", changeLanguage: () => Promise.resolve() },
        }),
    };
}
