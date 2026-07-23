import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';

export type SupportedLocale = 'en-US' | 'zh-CN';

type TranslationResource = Record<string, unknown>;
export type LocaleExtensionLoader = (
    locale: SupportedLocale,
) => Promise<TranslationResource>;

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
const localeExtensionLoads = new Map<
    LocaleExtensionLoader,
    Map<SupportedLocale, Promise<void>>
>();
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

async function ensureLocaleExtensionsLoaded(
    locale: SupportedLocale,
): Promise<void> {
    await Promise.all(
        [...localeExtensionLoads].map(async ([loader, localeLoads]) => {
            const existing = localeLoads.get(locale);
            if (existing) {
                await existing;
                return;
            }

            const loading = loader(locale).then((translation) => {
                i18n.addResourceBundle(
                    locale,
                    'translation',
                    translation,
                    true,
                    true,
                );
            });
            localeLoads.set(locale, loading);
            void loading.catch(() => {
                if (localeLoads.get(locale) === loading) {
                    localeLoads.delete(locale);
                }
            });
            await loading;
        }),
    );
}

export function registerLocaleExtension(
    loader: LocaleExtensionLoader,
): () => void {
    if (!localeExtensionLoads.has(loader)) {
        localeExtensionLoads.set(loader, new Map());
    }
    return () => {
        localeExtensionLoads.delete(loader);
    };
}

async function initialize(): Promise<void> {
    let initialLanguage = canonicalizeLocale(
        i18n.resolvedLanguage ?? i18n.language ?? '',
    );

    if (!i18n.isInitialized) {
        const savedLanguage = canonicalizeLocale(
            localStorage.getItem('i18nextLng') ?? '',
        );
        initialLanguage = savedLanguage ?? 'zh-CN';
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

    await ensureLocaleExtensionsLoaded(initialLanguage ?? 'zh-CN');
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
    if (!registeredBaseLocales.has(locale)) {
        const translation = await loadLocaleResource(locale);
        i18n.addResourceBundle(locale, 'translation', translation, true, false);
        registeredBaseLocales.add(locale);
    }
    await ensureLocaleExtensionsLoaded(locale);
}

export default i18n;
