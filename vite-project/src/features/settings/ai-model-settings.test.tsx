import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

// i18n: real en-US locale, interpolating {{name}} placeholders like i18next.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: h.toast }),
}))

// Mutable query payload + spy on the update mutation.
const h = vi.hoisted(() => ({
    publicData: {} as Record<string, unknown>,
    mutateAsync: vi.fn(async () => ({})),
    policyData: {} as Record<string, unknown>,
    policyMutateAsync: vi.fn(async () => ({})),
    // The validate mutation returns a RestResponse body; tests override it.
    validateMutateAsync: vi.fn(async () => ({ success: true, data: {} }) as Record<string, unknown>),
    toast: vi.fn(),
}))

vi.mock("@/services/hooks/aiModelController/useQueryAiModelSettings", () => ({
    useQueryAiModelSettings: () => ({ data: { data: h.publicData }, isLoading: false }),
}))
vi.mock("@/services/hooks/aiModelController/useUpdateAiModelSettings", () => ({
    useUpdateAiModelSettings: () => ({ mutateAsync: h.mutateAsync, isPending: false }),
}))
vi.mock("@/services/hooks/aiModelController/useValidateAiModelSettings", () => ({
    useValidateAiModelSettings: () => ({ mutateAsync: h.validateMutateAsync, isPending: false }),
}))
vi.mock("@/services/hooks/aiModelController/useQueryCollectionPolicySettings", () => ({
    useQueryCollectionPolicySettings: () => ({ data: { data: h.policyData }, isLoading: false }),
}))
vi.mock("@/services/hooks/aiModelController/useUpdateCollectionPolicySettings", () => ({
    useUpdateCollectionPolicySettings: () => ({ mutateAsync: h.policyMutateAsync, isPending: false }),
}))

import { AiModelSettings } from "./ai-model-settings"

function lastPayload() {
    return (h.mutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
}

beforeEach(() => {
    h.mutateAsync.mockClear()
    h.policyMutateAsync.mockClear()
    h.validateMutateAsync.mockClear()
    h.toast.mockClear()
    h.validateMutateAsync.mockResolvedValue({ success: true, data: {} })
    h.publicData = {
        provider: "openai-compatible",
        model: "gpt-4o-mini",
        base_url: "https://api.example/v1",
        max_context_bytes: 0,
        response_format: "json_schema",
        api_key_set: true,
    }
    h.policyData = { allow_screen: false, allow_logs: true }
})

describe("AiModelSettings", () => {
    it("hydrates from the query and saves the hydrated response_format, omitting a blank api_key and a zero budget", async () => {
        render(<AiModelSettings />)
        // Hydration populated the form from the masked public view.
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())

        const payload = lastPayload()
        expect(payload.response_format).toBe("json_schema")
        expect(payload.provider).toBe("openai-compatible")
        expect(payload.base_url).toBe("https://api.example/v1")
        // The gateway form no longer carries the collection policy.
        expect(payload.allow_logs).toBeUndefined()
        expect(payload.allow_screen).toBeUndefined()
        // Blank key field → leave unchanged (omitted); 0 budget → default (omitted).
        expect(payload.api_key).toBeUndefined()
        expect(payload.max_context_bytes).toBeUndefined()
    })

    it("saves the collection policy from its own form, hydrated from its query", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        // The second "Save Settings" button belongs to the collection-policy card.
        fireEvent.click(screen.getAllByText("Save Settings")[1])
        await waitFor(() => expect(h.policyMutateAsync).toHaveBeenCalled())

        const payload = (h.policyMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
        // Hydrated from policyData (allow_logs: true, allow_screen: false).
        expect(payload.allow_logs).toBe(true)
        expect(payload.allow_screen).toBe(false)
    })

    it("includes a typed api_key in the payload", async () => {
        render(<AiModelSettings />)
        const keyInput = await screen.findByPlaceholderText(/Configured/i)
        fireEvent.change(keyInput, { target: { value: "sk-typed" } })

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().api_key).toBe("sk-typed")
    })

    it("the clear toggle sends an empty api_key (clear semantics)", async () => {
        render(<AiModelSettings />)
        // The clear switch only renders when a key is already configured.
        const clearSwitch = await screen.findByRole("switch", { name: /clear stored key/i })
        fireEvent.click(clearSwitch)

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().api_key).toBe("")
    })

    it("hydrates the anthropic provider, shows its base-URL hint, and saves it", async () => {
        h.publicData = { ...h.publicData, provider: "anthropic" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // The provider-specific base URL hint warns against appending /v1.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().provider).toBe("anthropic")
    })

    it("recognizes a mixed-case / padded anthropic provider on hydration", async () => {
        h.publicData = { ...h.publicData, provider: "  Anthropic  " }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // Normalized to anthropic — its base-URL hint is shown, not silently
        // switched to openai-compatible.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().provider).toBe("anthropic")
    })

    it("normalizes an unknown stored provider to openai-compatible", async () => {
        h.publicData = { ...h.publicData, provider: "some-legacy-value" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().provider).toBe("openai-compatible")
    })

    it("validates the saved gateway and shows a success toast with provider / model", async () => {
        h.validateMutateAsync.mockResolvedValue({
            success: true,
            data: { provider: "openai-compatible", model: "gpt-4o-mini", adapter: "lcxl-openai-compat" },
        })
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getByText("Validate"))
        await waitFor(() => expect(h.validateMutateAsync).toHaveBeenCalled())
        await waitFor(() => expect(h.toast).toHaveBeenCalled())
        const arg = h.toast.mock.calls.at(-1)![0] as { variant?: string; description?: string }
        // A success toast is not the destructive variant and echoes the model.
        expect(arg.variant).toBeUndefined()
        expect(arg.description).toContain("gpt-4o-mini")
    })

    it("surfaces the gateway's own reason on a validation failure", async () => {
        h.validateMutateAsync.mockResolvedValue({
            success: false,
            code: 1,
            message: "model gateway returned status 400 Bad Request: unknown model",
        })
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getByText("Validate"))
        await waitFor(() => expect(h.toast).toHaveBeenCalled())
        const arg = h.toast.mock.calls.at(-1)![0] as { variant?: string; description?: string }
        expect(arg.variant).toBe("destructive")
        // The gateway's verbatim reason is shown so the operator can fix the config.
        expect(arg.description).toContain("status 400")
    })

    it("hides the clear toggle when no key is configured", async () => {
        h.publicData = { ...h.publicData, api_key_set: false, response_format: "json_object" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.queryByRole("switch", { name: /clear stored key/i })).toBeNull()
    })
})
