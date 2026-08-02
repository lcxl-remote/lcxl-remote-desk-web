import { act, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, useLocation } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)
vi.mock("@/components/mode-toggle", () => ({ ModeToggle: () => null }))
vi.mock("@/components/language-toggle", () => ({ LanguageToggle: () => null }))

const axiosPost = vi.hoisted(() => vi.fn())
vi.mock("axios", () => ({ default: { post: axiosPost } }))
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
        axiosPost.mockReset()
        fetchUserInfo.mockReset()
    })

    it("blocks normal login until auto-login, user refresh, and navigation complete", async () => {
        const login = deferred<{ data: { status: string; startup_mode: string } }>()
        const currentUser = deferred<unknown>()
        axiosPost.mockReturnValue(login.promise)
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

        act(() => login.resolve({ data: { status: "ok", startup_mode: "default" } }))
        await waitFor(() => expect(fetchUserInfo).toHaveBeenCalledTimes(1))
        expect(submit).toBeDisabled()
        expect(submit).toHaveTextContent("Signing in…")

        act(() => currentUser.resolve({}))
        await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/desk/list"))
        expect(axiosPost).toHaveBeenCalledTimes(1)
    })
})
