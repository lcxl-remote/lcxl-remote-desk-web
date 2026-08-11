import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, useLocation } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"
import { RestResponseError } from "@/lib/kubb-client"
import { deskErrorCodeEnum } from "@/services/types"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)
vi.mock("@/components/mode-toggle", () => ({ ModeToggle: () => null }))
vi.mock("@/components/language-toggle", () => ({ LanguageToggle: () => null }))

const tauriLogin = vi.hoisted(() => vi.fn())
vi.mock("@/services/hooks/authController/useLoginTauri", () => ({
    useLoginTauri: () => ({ mutateAsync: tauriLogin }),
}))
const fetchUserInfo = vi.hoisted(() => vi.fn())
const accountLogin = vi.hoisted(() => vi.fn())
vi.mock("@/services/hooks/authController/useLoginAccount", () => ({
    useLoginAccount: () => ({ mutateAsync: accountLogin }),
}))
vi.mock("@/services/hooks/authController/useRedeemCode", () => ({
    useRedeemCode: () => ({ mutateAsync: vi.fn() }),
}))
vi.mock("@/services/hooks/userController/useGetCurrentUser", () => ({
    useGetCurrentUser: () => ({ refetch: fetchUserInfo }),
}))
vi.mock("@/services/hooks/systemController/useQueryServerInfo", () => ({
    useQueryServerInfo: () => ({
        data: { data: { initialized: true, startup_mode: "default" } },
        isLoading: false,
    }),
}))
vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: vi.fn() }),
}))

import LoginPage from "./login-page"

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

function LocationProbe() {
    return <span data-testid="location">{useLocation().pathname}</span>
}

describe("LoginPage Tauri auto-login", () => {
    beforeEach(() => {
        tauriLogin.mockReset()
        fetchUserInfo.mockReset()
        accountLogin.mockReset()
    })

    it("blocks normal login until auto-login, user refresh, and navigation complete", async () => {
        const login = deferred<{ data: { startup_mode: string } }>()
        const currentUser = deferred<unknown>()
        tauriLogin.mockReturnValue(login.promise)
        fetchUserInfo.mockReturnValue(currentUser.promise)
        render(
            <MemoryRouter initialEntries={["/user/login?token=tauri-token"]}>
                <LoginPage />
                <LocationProbe />
            </MemoryRouter>,
        )
        const submit = await screen.findByRole("button", { name: "Signing in…" })
        expect(submit).toBeDisabled()
        expect(submit).toHaveAttribute("aria-busy", "true")

        expect(tauriLogin).toHaveBeenCalledWith({ params: { token: "tauri-token" } })
        act(() => login.resolve({ data: { startup_mode: "default" } }))
        await waitFor(() => expect(fetchUserInfo).toHaveBeenCalledTimes(1))
        expect(submit).toBeDisabled()
        expect(submit).toHaveTextContent("Signing in…")

        act(() => currentUser.resolve({}))
        await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/desk/list"))
        expect(tauriLogin).toHaveBeenCalledTimes(1)
    })

    it("returns to the normal form when the generated mutation rejects a business failure", async () => {
        tauriLogin.mockRejectedValue(new Error("invalid token"))
        render(
            <MemoryRouter initialEntries={["/user/login?token=bad-token"]}>
                <LoginPage />
            </MemoryRouter>,
        )

        await waitFor(() => expect(tauriLogin).toHaveBeenCalledWith({ params: { token: "bad-token" } }))
        await waitFor(() => expect(screen.getByRole("button", { name: "Login" })).toBeEnabled())
    })

    it("disables account login for the server-provided lock duration", async () => {
        const clearInterval = vi.spyOn(window, "clearInterval")
        accountLogin.mockRejectedValue(
            new RestResponseError("locked", deskErrorCodeEnum.ACCOUNT_LOCKED, {
                retry_after_sec: 1,
            }),
        )
        render(
            <MemoryRouter initialEntries={["/user/login"]}>
                <LoginPage />
            </MemoryRouter>,
        )

        fireEvent.change(screen.getByPlaceholderText("Please input your username"), {
            target: { value: "admin" },
        })
        fireEvent.change(screen.getByPlaceholderText("Please input your password"), {
            target: { value: "password" },
        })
        fireEvent.click(screen.getByRole("button", { name: "Login" }))

        await waitFor(() => expect(accountLogin).toHaveBeenCalledTimes(1))
        expect(screen.getByRole("button", { name: "Try again in 1s" })).toBeDisabled()
        await waitFor(() => expect(screen.getByRole("button", { name: "Login" })).toBeEnabled(), {
            timeout: 2500,
        })
        expect(clearInterval).toHaveBeenCalled()
        clearInterval.mockRestore()
    })
})
