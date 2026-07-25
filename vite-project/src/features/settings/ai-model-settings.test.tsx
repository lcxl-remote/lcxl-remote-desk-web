import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

// i18n: real en-US locale, interpolating {{name}} placeholders like i18next.
vi.mock("react-i18next", () => import("@/test-utils/i18n-mock").then((m) => m.reactI18nextMock()))

vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: h.toast }),
}))

// Mutable central model-provider query payload + update spy.
const h = vi.hoisted(() => ({
    providerData: {} as Record<string, unknown>,
    providerMutateAsync: vi.fn(async () => ({})),
    toast: vi.fn(),
}))

vi.mock("@/services/hooks/modelProviderController/useGetModelProvider", () => ({
    useGetModelProvider: () => ({ data: { data: h.providerData }, isLoading: false }),
}))
vi.mock("@/services/hooks/modelProviderController/useUpdateModelProvider", () => ({
    useUpdateModelProvider: () => ({ mutateAsync: h.providerMutateAsync, isPending: false }),
}))
vi.mock("@/services/hooks/modelProviderController/useTestModelProvider", () => ({
    useTestModelProvider: () => ({ mutateAsync: vi.fn(), isPending: false }),
}))

import { AiModelSettings } from "./ai-model-settings"

function lastProviderPayload() {
    return (h.providerMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
}

beforeEach(() => {
    h.providerMutateAsync.mockClear()
    h.toast.mockClear()
    h.providerData = {
        provider: "openai-compatible",
        model: "gpt-4o-mini",
        base_url: "https://api.example/v1",
        max_context_bytes: 0,
        response_format: "json_schema",
        execution_mode: "read_only",
        max_steps_per_turn: 20,
        max_same_tool_calls_per_turn: 10,
        api_key_set: true,
    }
})

describe("AiModelSettings", () => {
    it("hydrates the provider form and saves it, omitting a blank api_key and a zero budget", async () => {
        render(<AiModelSettings />)
        // Hydration populated the form from the masked public view.
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        const payload = lastProviderPayload()
        expect(payload.response_format).toBe("json_schema")
        expect(payload.provider).toBe("openai-compatible")
        expect(payload.base_url).toBe("https://api.example/v1")
        // The central grant carries the execution mode.
        expect(payload.execution_mode).toBe("read_only")
        expect(payload.max_steps_per_turn).toBe(20)
        expect(payload.max_same_tool_calls_per_turn).toBe(10)
        // The provider form no longer carries the collection policy.
        expect(payload.allow_logs).toBeUndefined()
        expect(payload.allow_screen).toBeUndefined()
        // Blank key field → leave unchanged (omitted); 0 budget → default (omitted).
        expect(payload.api_key).toBeUndefined()
        expect(payload.max_context_bytes).toBeUndefined()
    })

    it("rejects a reasoning-round limit below the same-tool limit", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.change(screen.getByLabelText("Model reasoning rounds per turn"), {
            target: { value: "9" },
        })
        fireEvent.click(screen.getAllByText("Save Settings")[0])

        expect(
            await screen.findByText(
                "The model reasoning-round limit cannot be lower than the same-tool call limit.",
            ),
        ).toBeInTheDocument()
        expect(h.providerMutateAsync).not.toHaveBeenCalled()
    })

    it("includes a typed api_key in the provider payload", async () => {
        render(<AiModelSettings />)
        const keyInput = await screen.findByPlaceholderText(/Configured/i)
        fireEvent.change(keyInput, { target: { value: "sk-typed" } })

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().api_key).toBe("sk-typed")
    })

    it("the clear toggle sends an empty api_key (clear semantics)", async () => {
        render(<AiModelSettings />)
        // The clear switch only renders when a key is already configured.
        const clearSwitch = await screen.findByRole("switch", { name: /clear stored key/i })
        fireEvent.click(clearSwitch)

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().api_key).toBe("")
    })

    it("hydrates the anthropic provider, shows its base-URL hint, and saves it", async () => {
        h.providerData = { ...h.providerData, provider: "anthropic" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // The provider-specific base URL hint warns against appending /v1.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().provider).toBe("anthropic")
    })

    it("recognizes a mixed-case / padded anthropic provider on hydration", async () => {
        h.providerData = { ...h.providerData, provider: "  Anthropic  " }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // Normalized to anthropic — its base-URL hint is shown, not silently
        // switched to openai-compatible.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().provider).toBe("anthropic")
    })

    it("normalizes an unknown stored provider to openai-compatible", async () => {
        h.providerData = { ...h.providerData, provider: "some-legacy-value" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().provider).toBe("openai-compatible")
    })

    it("hides the clear toggle when no key is configured", async () => {
        h.providerData = { ...h.providerData, api_key_set: false, response_format: "json_object" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.queryByRole("switch", { name: /clear stored key/i })).toBeNull()
    })
})
