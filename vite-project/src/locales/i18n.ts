import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';

export type SupportedLocale = 'en-US' | 'zh-CN';

type TranslationResource = Record<string, unknown>;

const localeLoaders: Record<
    SupportedLocale,
    () => Promise<TranslationResource>
> = {
    'en-US': async () => (await import('./en-US')).default,
    'zh-CN': async () => (await import('./zh-CN')).default,
};

const localeResources = new Map<
    SupportedLocale,
    Promise<TranslationResource>
>();
const registeredBaseLocales = new Set<SupportedLocale>();
let initialization: Promise<void> | undefined;

export function canonicalizeLocale(locale: string): SupportedLocale | null {
    switch (locale.trim().toLowerCase()) {
        case 'en':
        case 'en-us':
        case 'en_us':
            return 'en-US';
        case 'zh':
        case 'zh-cn':
        case 'zh_cn':
        case 'zh-hans':
            return 'zh-CN';
        default:
            return null;
    }
}

function loadLocaleResource(
    locale: SupportedLocale,
): Promise<TranslationResource> {
    const existing = localeResources.get(locale);
    if (existing) return existing;

    const loading = localeLoaders[locale]();
    localeResources.set(locale, loading);
    void loading.catch(() => {
        if (localeResources.get(locale) === loading) {
            localeResources.delete(locale);
        }
    });
    return loading;
}

async function initialize(): Promise<void> {
    if (i18n.isInitialized) return;

    const savedLanguage = canonicalizeLocale(
        localStorage.getItem('i18nextLng') ?? '',
    );
    const initialLanguage = savedLanguage ?? 'zh-CN';
    const translation = await loadLocaleResource(initialLanguage);

    await i18n.use(initReactI18next).init({
        resources: {
            [initialLanguage]: {
                translation,
            },
        },
        lng: initialLanguage,
        fallbackLng: 'en-US',
        interpolation: {
            escapeValue: false,
        },
    });
    registeredBaseLocales.add(initialLanguage);
}

export function initializeI18n(): Promise<void> {
    if (!initialization) {
        initialization = initialize().catch((error: unknown) => {
            initialization = undefined;
            throw error;
        });
    }
    return initialization;
}

export async function ensureLocaleLoaded(
    locale: SupportedLocale,
): Promise<void> {
    await initializeI18n();
    if (registeredBaseLocales.has(locale)) return;

    const translation = await loadLocaleResource(locale);
    i18n.addResourceBundle(locale, 'translation', translation, true, false);
    registeredBaseLocales.add(locale);
}

export default i18n;
