import { beforeEach, describe, expect, it, vi } from 'vitest'

const { changeLanguage, ensureLocaleLoaded } = vi.hoisted(() => ({
    changeLanguage: vi.fn().mockResolvedValue(undefined),
    ensureLocaleLoaded: vi.fn().mockResolvedValue(undefined),
}))
vi.mock('./i18n', () => ({
    canonicalizeLocale: (locale: string) => {
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
    },
    default: {
        changeLanguage,
        language: 'en-US',
        resolvedLanguage: 'en-US',
    },
    ensureLocaleLoaded,
}))

import {
    TAURI_SHELL_SESSION_KEY,
    changeApplicationLanguage,
    initializeNativeLocaleBridge,
} from './native-locale'

beforeEach(() => {
    localStorage.clear()
    sessionStorage.clear()
    changeLanguage.mockClear()
    ensureLocaleLoaded.mockClear()
    vi.unstubAllGlobals()
})

describe('changeApplicationLanguage', () => {
    it('keeps a normal browser language change entirely local', async () => {
        const fetchMock = vi.fn()
        vi.stubGlobal('fetch', fetchMock)

        await changeApplicationLanguage('en-US')

        expect(fetchMock).not.toHaveBeenCalled()
        expect(localStorage.getItem('i18nextLng')).toBe('en-US')
        expect(ensureLocaleLoaded).toHaveBeenCalledWith('en-US')
        expect(changeLanguage).toHaveBeenCalledWith('en-US')
    })

    it('commits through the native bridge before changing a Tauri page', async () => {
        sessionStorage.setItem(TAURI_SHELL_SESSION_KEY, '1')
        sessionStorage.setItem('lcxl.nativeBridgeToken', 'session-token')
        const fetchMock = vi.fn().mockResolvedValue(
            new Response(JSON.stringify({ locale: 'en-US' }), {
                status: 200,
                headers: { 'Content-Type': 'application/json' },
            }),
        )
        vi.stubGlobal('fetch', fetchMock)

        await changeApplicationLanguage('en-US')

        expect(fetchMock).toHaveBeenCalledWith('/api/native/locale', {
            method: 'PUT',
            headers: {
                Authorization: 'Bearer session-token',
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({ locale: 'en-US' }),
        })
        expect(changeLanguage).toHaveBeenCalledWith('en-US')
    })

    it('does not silently fall back to a web-only change when native commit fails', async () => {
        sessionStorage.setItem(TAURI_SHELL_SESSION_KEY, '1')
        sessionStorage.setItem('lcxl.nativeBridgeToken', 'session-token')
        vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('', { status: 500 })))

        await expect(changeApplicationLanguage('en-US')).rejects.toThrow(
            'native locale update failed',
        )
        expect(localStorage.getItem('i18nextLng')).toBeNull()
        expect(changeLanguage).not.toHaveBeenCalled()
    })

    it('uses the existing web locale to initialize an unconfigured desktop shell', async () => {
        const fetchMock = vi.fn().mockResolvedValue(
            new Response(JSON.stringify({ locale: 'en-US' }), {
                status: 200,
                headers: { 'Content-Type': 'application/json' },
            }),
        )
        vi.stubGlobal('fetch', fetchMock)
        initializeNativeLocaleBridge()

        window.dispatchEvent(
            new CustomEvent('lcxl-native-bridge-ready', {
                detail: {
                    token: 'first-session',
                    locale: 'zh-CN',
                    localePersisted: false,
                },
            }),
        )

        await vi.waitFor(() => expect(fetchMock).toHaveBeenCalled())
        expect(fetchMock.mock.calls[0][1].body).toBe(
            JSON.stringify({ locale: 'en-US' }),
        )
    })
})
