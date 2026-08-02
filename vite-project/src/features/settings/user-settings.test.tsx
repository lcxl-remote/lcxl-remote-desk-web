import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)

const updateCredentials = vi.hoisted(() => vi.fn())
vi.mock("@/services/hooks/authController/useChangePassword", () => ({
    useChangePassword: () => ({ mutateAsync: updateCredentials, isPending: false }),
}))
vi.mock("@/services/hooks/userController/useGetCurrentUser", () => ({
    useGetCurrentUser: () => ({ data: { data: { name: "admin", user_id: null } } }),
}))
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast: vi.fn() }) }))

import { UserSettings } from "./user-settings"

describe("UserSettings", () => {
    beforeEach(() => updateCredentials.mockReset())

    it("submits the canonical credentials request", async () => {
        updateCredentials.mockResolvedValue({ success: true, data: null })
        render(<UserSettings />)

        fireEvent.change(screen.getByLabelText("Current Password"), { target: { value: "old-secret" } })
        fireEvent.change(screen.getByLabelText("New Password"), { target: { value: "new-secret-123" } })
        fireEvent.change(screen.getByLabelText("Confirm New Password"), { target: { value: "new-secret-123" } })
        fireEvent.click(screen.getByRole("button", { name: "Update Password" }))

        await waitFor(() => expect(updateCredentials).toHaveBeenCalledWith({
            data: {
                current_username: "admin",
                current_password: "old-secret",
                new_username: null,
                new_password: "new-secret-123",
            },
        }))
    })
})
