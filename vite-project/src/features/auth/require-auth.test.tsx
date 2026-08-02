import { render, screen } from "@testing-library/react"
import { MemoryRouter, Route, Routes } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

const currentUserResult = vi.hoisted(() => vi.fn())
vi.mock("@/services/hooks/userController/useGetCurrentUser", () => ({
    useGetCurrentUser: currentUserResult,
}))

import RequireAuth from "./require-auth"

function renderGuard() {
    return render(
        <MemoryRouter initialEntries={["/private?token=one"]}>
            <Routes>
                <Route
                    path="/private"
                    element={<RequireAuth><span>private content</span></RequireAuth>}
                />
                <Route path="/user/login" element={<span>login page</span>} />
            </Routes>
        </MemoryRouter>,
    )
}

describe("RequireAuth", () => {
    beforeEach(() => currentUserResult.mockReset())

    it("renders protected content from the current-user envelope", () => {
        currentUserResult.mockReturnValue({
            data: { data: { name: "admin", access: "admin" } },
            isLoading: false,
            isError: false,
        })

        renderGuard()

        expect(screen.getByText("private content")).toBeInTheDocument()
    })

    it("redirects after the generated query reports an HTTP 401 error", () => {
        currentUserResult.mockReturnValue({ data: undefined, isLoading: false, isError: true })

        renderGuard()

        expect(screen.getByText("login page")).toBeInTheDocument()
    })
})
