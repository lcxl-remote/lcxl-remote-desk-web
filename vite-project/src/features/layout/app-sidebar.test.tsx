import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { MemoryRouter, useLocation } from "react-router-dom"
import { beforeEach, describe, expect, it, vi } from "vitest"

vi.mock("react-i18next", () =>
    import("@/test-utils/i18n-mock").then((module) => module.reactI18nextMock()),
)
vi.mock("@/components/ui/sidebar", () => {
    const Wrapper = ({ children }: { children?: React.ReactNode }) => <div>{children}</div>
    const MenuButton = ({ asChild, children, ...props }: {
        asChild?: boolean
        children?: React.ReactNode
        [key: string]: unknown
    }) => asChild ? children : <button {...props}>{children}</button>
    return {
        Sidebar: Wrapper,
        SidebarContent: Wrapper,
        SidebarFooter: Wrapper,
        SidebarHeader: Wrapper,
        SidebarMenu: Wrapper,
        SidebarMenuButton: MenuButton,
        SidebarMenuItem: Wrapper,
        SidebarMenuSub: Wrapper,
        SidebarMenuSubButton: MenuButton,
        SidebarMenuSubItem: Wrapper,
        SidebarRail: () => null,
    }
})
vi.mock("@/components/ui/collapsible", () => ({
    Collapsible: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    CollapsibleContent: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    CollapsibleTrigger: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}))
vi.mock("@/components/ui/dropdown-menu", () => ({
    DropdownMenu: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    DropdownMenuContent: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    DropdownMenuLabel: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    DropdownMenuSeparator: () => null,
    DropdownMenuTrigger: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
    DropdownMenuItem: ({ asChild, children, ...props }: {
        asChild?: boolean
        children?: React.ReactNode
        [key: string]: unknown
    }) => asChild ? children : <button {...props}>{children}</button>,
}))
vi.mock("@/components/ui/avatar", () => ({
    Avatar: ({ children }: { children?: React.ReactNode }) => <div>{children}</div>,
    AvatarFallback: ({ children }: { children?: React.ReactNode }) => <span>{children}</span>,
    AvatarImage: () => null,
}))
vi.mock("@/services/hooks/systemController/useQueryServerInfo", () => ({
    useQueryServerInfo: () => ({ data: { data: { startup_mode: "default" } } }),
}))
vi.mock("@/services/hooks/userController/useGetCurrentUser", () => ({
    useGetCurrentUser: () => ({ data: { data: { name: "Alice", access: "user" } } }),
}))

const logout = vi.hoisted(() => vi.fn())
vi.mock("@/services/hooks/authController/useLogoutAccount", () => ({
    useLogoutAccount: () => ({ mutateAsync: logout }),
}))
const clearAllGrants = vi.hoisted(() => vi.fn())
vi.mock("@/features/desk/session-grant", () => ({ clearAllGrants }))
vi.mock("@/hooks/use-toast", () => ({ useToast: () => ({ toast: vi.fn() }) }))
vi.mock("@/features/layout/sidebar-nav", () => ({
    buildNavItems: () => [],
    startupModeLabel: () => "Default",
}))

import { AppSidebar } from "./app-sidebar"

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

describe("AppSidebar logout", () => {
    beforeEach(() => {
        logout.mockReset()
        clearAllGrants.mockReset()
    })

    it("waits for server logout, guards re-entry, then navigates", async () => {
        const request = deferred<void>()
        logout.mockReturnValue(request.promise)
        render(
            <MemoryRouter initialEntries={["/desk/list"]}>
                <AppSidebar />
                <LocationProbe />
            </MemoryRouter>,
        )
        const button = screen.getByRole("button", { name: /logout/i })

        fireEvent.click(button)
        fireEvent.click(button)

        expect(clearAllGrants).toHaveBeenCalledTimes(1)
        expect(logout).toHaveBeenCalledTimes(1)
        expect(button).toBeDisabled()
        expect(button).toHaveAttribute("aria-busy", "true")
        expect(button).toHaveTextContent("Signing out…")
        expect(screen.getByTestId("location")).toHaveTextContent("/desk/list")

        act(() => request.resolve())
        await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/user/login"))
    })
})
