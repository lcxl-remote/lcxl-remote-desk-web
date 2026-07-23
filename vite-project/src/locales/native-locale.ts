import i18n from './i18n'

export const TAURI_SHELL_SESSION_KEY = 'lcxl.tauriShell'
const NATIVE_BRIDGE_TOKEN_KEY = 'lcxl.nativeBridgeToken'
const BRIDGE_READY_EVENT = 'lcxl-native-bridge-ready'
const GLOBAL_LOCALE_EVENT = 'lcxl-global-locale-changed'

type SupportedLocale = 'en-US' | 'zh-CN'

function canonicalizeLocale(locale: string): SupportedLocale | null {
    switch (locale.trim().toLowerCase()) {
        case 'en':
        case 'en-us':
        case 'en_us':
            return 'en-US'
        case 'zh':
        case 'zh-cn':
        case 'zh_cn':
        case 'zh-hans':
            return 'zh-CN'
        default:
            return null
    }
}

function rememberShellMarker(): void {
    try {
        if (new URLSearchParams(window.location.search).get('tauri') === '1') {
            sessionStorage.setItem(TAURI_SHELL_SESSION_KEY, '1')
        }
    } catch {
        // Non-browser test/runtime.
    }
}

export function isTauriShell(): boolean {
    rememberShellMarker()
    try {
        return sessionStorage.getItem(TAURI_SHELL_SESSION_KEY) === '1'
    } catch {
        return false
    }
}

async function applyAuthoritativeLocale(locale: string): Promise<void> {
    const canonical = canonicalizeLocale(locale)
    if (!canonical) return
    localStorage.setItem('i18nextLng', canonical)
    await i18n.changeLanguage(canonical)
}

function bridgeToken(): string | null {
    try {
        return sessionStorage.getItem(NATIVE_BRIDGE_TOKEN_KEY)
    } catch {
        return null
    }
}

function waitForBridgeToken(timeoutMs = 5000): Promise<string> {
    const current = bridgeToken()
    if (current) return Promise.resolve(current)

    return new Promise((resolve, reject) => {
        const timeout = window.setTimeout(() => {
            window.removeEventListener(BRIDGE_READY_EVENT, onReady)
            reject(new Error('native locale bridge is not ready'))
        }, timeoutMs)
        const onReady = (event: Event) => {
            const detail = (event as CustomEvent<{ token?: string }>).detail
            if (!detail?.token) return
            window.clearTimeout(timeout)
            window.removeEventListener(BRIDGE_READY_EVENT, onReady)
            resolve(detail.token)
        }
        window.addEventListener(BRIDGE_READY_EVENT, onReady)
    })
}

export function initializeNativeLocaleBridge(): void {
    rememberShellMarker()
    window.addEventListener(BRIDGE_READY_EVENT, (event) => {
        const detail = (event as CustomEvent<{
            token?: string
            locale?: string
            localePersisted?: boolean
        }>).detail
        if (detail?.token) {
            sessionStorage.setItem(NATIVE_BRIDGE_TOKEN_KEY, detail.token)
            sessionStorage.setItem(TAURI_SHELL_SESSION_KEY, '1')
        }
        if (detail?.localePersisted === false) {
            void changeApplicationLanguage(i18n.resolvedLanguage ?? i18n.language)
        } else if (detail?.locale) {
            void applyAuthoritativeLocale(detail.locale)
        }
    })
    window.addEventListener(GLOBAL_LOCALE_EVENT, (event) => {
        const locale = (event as CustomEvent<{ locale?: string }>).detail?.locale
        if (locale) void applyAuthoritativeLocale(locale)
    })
}

export async function changeApplicationLanguage(locale: string): Promise<void> {
    const canonical = canonicalizeLocale(locale)
    if (!canonical) throw new Error(`unsupported locale: ${locale}`)

    if (!isTauriShell()) {
        await applyAuthoritativeLocale(canonical)
        return
    }

    const token = await waitForBridgeToken()
    const response = await fetch('/api/native/locale', {
        method: 'PUT',
        headers: {
            Authorization: `Bearer ${token}`,
            'Content-Type': 'application/json',
        },
        body: JSON.stringify({ locale: canonical }),
    })
    if (!response.ok) {
        throw new Error(`native locale update failed (${response.status})`)
    }
    const result = (await response.json()) as { locale?: string }
    await applyAuthoritativeLocale(result.locale ?? canonical)
}
