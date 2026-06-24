import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

// i18n: real en-US locale so assertions match what users see.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

// Mock the generated server-info hook so the page renders in
// "service-daemon mode with admin" without exercising TanStack Query.
vi.mock("@/services/hooks/systemController/useQueryServerInfo", () => ({
    useQueryServerInfo: () => ({
        data: {
            data: {
                startup_mode: "service-daemon",
                is_admin: true,
                service_installed: true,
            },
        },
    }),
}))

// Mock the toast hook — we don't assert toast firing; it's noise here.
vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: () => undefined }),
}))

// The component imports ServiceUninstallDialog; render is fine but
// shaving its TanStack Query dependencies keeps the test isolated.
vi.mock("@/features/layout/service-uninstall-dialog", () => ({
    ServiceUninstallDialog: () => null,
}))

import { VirtualDisplaySettings } from "./virtual-display-settings"

type FetchHandler = (url: string, init?: RequestInit) => unknown

function mockFetch(handler: FetchHandler) {
    global.fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : (input as URL).toString()
        const body = handler(url, init)
        return {
            ok: true,
            status: 200,
            json: async () => body,
        } as Response
    }) as unknown as typeof fetch
}

const INSTALLED_STATUS = {
    files_available: true,
    files_dir: "C:\\lcxl\\drivers\\LcxlVirtualDisplay",
    installed: true,
    installed_oem_infs: ["oem99.inf"],
    can_modify: true,
}

const DEFAULT_SETTINGS = {
    enabled: false,
    exclusive: false,
    prompt_ms: 5000,
    adaptive_debounce_ms: 5000,
    adaptive_throttle_ms: 1000,
    adaptive_min_delta_px: 16,
}

afterEach(() => {
    vi.restoreAllMocks()
})

describe("VirtualDisplaySettings — exclusive controls", () => {
    beforeEach(() => {
        // Default fetch handlers — individual tests can override.
        mockFetch((url) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                return { code: 0, data: { ...DEFAULT_SETTINGS, enabled: true } }
            }
            return { code: 0, data: null }
        })
    })

    it("disables exclusive toggle when virtual display is not enabled", async () => {
        // Override the settings GET to return enabled=false.
        mockFetch((url) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                return { code: 0, data: DEFAULT_SETTINGS } // enabled: false
            }
            return { code: 0, data: null }
        })
        render(<VirtualDisplaySettings />)

        const exclusiveSwitch = await screen.findByRole("switch", {
            name: /enable exclusive mode/i,
        })
        expect(exclusiveSwitch).toBeDisabled()
    })

    it("enables exclusive controls when virtual display is enabled", async () => {
        render(<VirtualDisplaySettings />)
        const exclusiveSwitch = await screen.findByRole("switch", {
            name: /enable exclusive mode/i,
        })
        await waitFor(() => {
            expect(exclusiveSwitch).not.toBeDisabled()
        })
        const promptInput = screen.getByLabelText(/pre-switch prompt duration/i)
        expect(promptInput).not.toBeDisabled()
    })

    it("toggling exclusive POSTs the full settings payload (no field reset)", async () => {
        const sent: Array<Record<string, unknown>> = []
        let getCount = 0
        mockFetch((url, init) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                if (init?.method === "POST") {
                    sent.push(JSON.parse(init.body as string))
                    const body = JSON.parse(init.body as string)
                    return { code: 0, data: body }
                }
                getCount += 1
                return { code: 0, data: { ...DEFAULT_SETTINGS, enabled: true } }
            }
            return { code: 0, data: null }
        })

        render(<VirtualDisplaySettings />)
        const exclusiveSwitch = await screen.findByRole("switch", {
            name: /enable exclusive mode/i,
        })
        await waitFor(() => {
            expect(exclusiveSwitch).not.toBeDisabled()
        })

        fireEvent.click(exclusiveSwitch)
        await waitFor(() => {
            expect(sent.length).toBe(1)
        })
        // Critical regression: every save must include the full
        // settings shape so the backend's serde::Default doesn't
        // reset adaptive_* / prompt_ms.
        expect(sent[0]).toEqual({
            enabled: true,
            exclusive: true,
            prompt_ms: 5000,
            adaptive_debounce_ms: 5000,
            adaptive_throttle_ms: 1000,
            adaptive_min_delta_px: 16,
        })
        expect(getCount).toBeGreaterThan(0) // initial GET fired
    })

    it("prompt_ms input onBlur clamps a value above 60000 down to 60000", async () => {
        const sent: Array<Record<string, unknown>> = []
        mockFetch((url, init) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                if (init?.method === "POST") {
                    const body = JSON.parse(init.body as string)
                    sent.push(body)
                    return { code: 0, data: body }
                }
                return { code: 0, data: { ...DEFAULT_SETTINGS, enabled: true } }
            }
            return { code: 0, data: null }
        })

        render(<VirtualDisplaySettings />)
        const promptInput = (await screen.findByLabelText(
            /pre-switch prompt duration/i,
        )) as HTMLInputElement
        await waitFor(() => {
            expect(promptInput).not.toBeDisabled()
        })

        fireEvent.change(promptInput, { target: { value: "60001" } })
        fireEvent.blur(promptInput)

        await waitFor(() => {
            expect(sent.length).toBe(1)
        })
        expect(sent[0].prompt_ms).toBe(60000)
        // The clamp also updates the visible input value.
        expect(promptInput.value).toBe("60000")
    })

    it("prompt_ms empty input onBlur reverts to current value without firing POST", async () => {
        const sent: Array<Record<string, unknown>> = []
        mockFetch((url, init) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                if (init?.method === "POST") {
                    const body = JSON.parse(init.body as string)
                    sent.push(body)
                    return { code: 0, data: body }
                }
                return { code: 0, data: { ...DEFAULT_SETTINGS, enabled: true } }
            }
            return { code: 0, data: null }
        })

        render(<VirtualDisplaySettings />)
        const promptInput = (await screen.findByLabelText(
            /pre-switch prompt duration/i,
        )) as HTMLInputElement
        await waitFor(() => {
            expect(promptInput).not.toBeDisabled()
        })
        // Initial value reflects the GET result (5000).
        expect(promptInput.value).toBe("5000")

        fireEvent.change(promptInput, { target: { value: "" } })
        fireEvent.blur(promptInput)

        // No POST should fire — empty buffer is treated as "user cleared
        // the field mid-edit", revert without saving.
        expect(sent).toEqual([])
        expect(promptInput.value).toBe("5000")
    })

    it("prompt_ms non-numeric input onBlur reverts to current value without firing POST", async () => {
        const sent: Array<Record<string, unknown>> = []
        mockFetch((url, init) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                if (init?.method === "POST") {
                    const body = JSON.parse(init.body as string)
                    sent.push(body)
                    return { code: 0, data: body }
                }
                return { code: 0, data: { ...DEFAULT_SETTINGS, enabled: true } }
            }
            return { code: 0, data: null }
        })

        render(<VirtualDisplaySettings />)
        const promptInput = (await screen.findByLabelText(
            /pre-switch prompt duration/i,
        )) as HTMLInputElement
        await waitFor(() => {
            expect(promptInput).not.toBeDisabled()
        })

        // The HTML number input strips truly non-numeric characters, but
        // we still cover the "isFinite false" branch by passing 'NaN'
        // string straight through the controlled state.
        fireEvent.change(promptInput, { target: { value: "abc" } })
        fireEvent.blur(promptInput)

        expect(sent).toEqual([])
        expect(promptInput.value).toBe("5000")
    })

    it("GET failure surfaces a retry alert and disables every save control", async () => {
        mockFetch((url) => {
            if (url.endsWith("/api/virtual-display/driver/status")) {
                return { code: 0, data: INSTALLED_STATUS }
            }
            if (url.endsWith("/api/desk/settings/virtual-display")) {
                // Simulate a non-zero code: backend reachable but returned an error.
                return { code: 5, data: null }
            }
            return { code: 0, data: null }
        })

        render(<VirtualDisplaySettings />)
        // Retry alert appears.
        await screen.findByText(/failed to load settings/i)
        const retryButton = screen.getByRole("button", { name: /retry/i })
        expect(retryButton).toBeInTheDocument()

        // All save controls are disabled — the enabled toggle, the
        // exclusive toggle, and the prompt_ms input.
        const enabledSwitch = screen.getByRole("switch", {
            name: /create virtual monitor on startup/i,
        })
        const exclusiveSwitch = screen.getByRole("switch", {
            name: /enable exclusive mode/i,
        })
        const promptInput = screen.getByLabelText(/pre-switch prompt duration/i)
        expect(enabledSwitch).toBeDisabled()
        expect(exclusiveSwitch).toBeDisabled()
        expect(promptInput).toBeDisabled()
    })
})
