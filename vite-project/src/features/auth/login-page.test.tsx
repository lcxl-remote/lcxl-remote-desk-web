import { act, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, useLocation } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

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
vi.mock("@/services/hooks/authController/useLoginAccount", () => ({
    useLoginAccount: () => ({ mutateAsync: vi.fn() }),
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
})
