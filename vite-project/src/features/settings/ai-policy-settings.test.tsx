import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: h.toast }),
}))

const h = vi.hoisted(() => ({
    policyData: {} as Record<string, unknown>,
    policyMutateAsync: vi.fn(async () => ({})),
    collectionData: {} as Record<string, unknown>,
    collectionMutateAsync: vi.fn(async () => ({})),
    toast: vi.fn(),
}))

vi.mock("@/services/hooks/aiModelController/useQueryAiPolicySettings", () => ({
    useQueryAiPolicySettings: () => ({ data: { data: h.policyData }, isLoading: false }),
}))
vi.mock("@/services/hooks/aiModelController/useUpdateAiPolicySettings", () => ({
    useUpdateAiPolicySettings: () => ({ mutateAsync: h.policyMutateAsync, isPending: false }),
}))
vi.mock("@/services/hooks/aiModelController/useQueryCollectionPolicySettings", () => ({
    useQueryCollectionPolicySettings: () => ({ data: { data: h.collectionData }, isLoading: false }),
}))
vi.mock("@/services/hooks/aiModelController/useUpdateCollectionPolicySettings", () => ({
    useUpdateCollectionPolicySettings: () => ({ mutateAsync: h.collectionMutateAsync, isPending: false }),
}))

import { AiPolicySettings } from "./ai-policy-settings"

beforeEach(() => {
    h.policyMutateAsync.mockClear()
    h.collectionMutateAsync.mockClear()
    h.toast.mockClear()
    h.policyData = {
        execution_mode: "confirm_each_action",
        max_concurrent_executions: 7,
        max_command_runtime_seconds: 600,
        exec_pty_enabled: true,
        interactive_elevation_enabled: false,
        exec_pty_supported: true,
        interactive_elevation_supported: false,
    }
    h.collectionData = { allow_screen: false, allow_logs: true }
})

describe("AiPolicySettings", () => {
    it("saves the Desk Server execution ceiling from its own form", async () => {
        render(<AiPolicySettings />)
        await waitFor(() => expect(screen.getByDisplayValue("7")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.policyMutateAsync).toHaveBeenCalled())

        const payload = (h.policyMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
        expect(payload.execution_mode).toBe("confirm_each_action")
        expect(payload.max_concurrent_executions).toBe(7)
        expect(payload.exec_pty_enabled).toBe(true)
        expect(payload.interactive_elevation_enabled).toBe(false)
    })

    it("resets elevation when ordinary PTY is disabled", async () => {
        h.policyData.interactive_elevation_enabled = true
        h.policyData.interactive_elevation_supported = true
        render(<AiPolicySettings />)

        const ptySwitch = await screen.findByRole("switch", { name: "One-shot PTY execution" })
        const elevationSwitch = screen.getByRole("switch", { name: "Interactive elevation" })
        expect(ptySwitch).toBeChecked()
        expect(elevationSwitch).toBeChecked()

        fireEvent.click(ptySwitch)
        expect(elevationSwitch).not.toBeChecked()
        expect(elevationSwitch).toBeDisabled()
    })

    it("shows elevation as unavailable when root containment is not ready", async () => {
        render(<AiPolicySettings />)

        const elevationSwitch = await screen.findByRole("switch", { name: "Interactive elevation" })
        expect(elevationSwitch).toBeDisabled()
        expect(
            screen.getByText("Unavailable unless this host is a root Linux ServiceDaemon with ready systemd containment."),
        ).toBeInTheDocument()
    })

    it("saves the Desk Server evidence collection policy separately", async () => {
        render(<AiPolicySettings />)
        await waitFor(() => expect(screen.getByDisplayValue("7")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[1])
        await waitFor(() => expect(h.collectionMutateAsync).toHaveBeenCalled())

        const payload = (h.collectionMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
        expect(payload.allow_logs).toBe(true)
        expect(payload.allow_screen).toBe(false)
    })
})
