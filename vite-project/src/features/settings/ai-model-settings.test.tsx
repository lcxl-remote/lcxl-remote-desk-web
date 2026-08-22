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
    testMutateAsync: vi.fn(async () => ({})),
    toast: vi.fn(),
}))

vi.mock("@/services/hooks/modelProviderController/useGetModelProvider", () => ({
    useGetModelProvider: () => ({ data: { data: h.providerData }, isLoading: false }),
}))
vi.mock("@/services/hooks/modelProviderController/useUpdateModelProvider", () => ({
    useUpdateModelProvider: () => ({ mutateAsync: h.providerMutateAsync, isPending: false }),
}))
vi.mock("@/services/hooks/modelProviderController/useTestModelProvider", () => ({
    useTestModelProvider: () => ({ mutateAsync: h.testMutateAsync, isPending: false }),
}))

import {
    AiModelSettings,
    isProfilePresetSelectable,
    presetProfile,
} from "./ai-model-settings"
import { RestResponseError } from "@/lib/kubb-client"

function lastProviderPayload() {
    return (h.providerMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
}

beforeEach(() => {
    h.providerMutateAsync.mockClear()
    h.testMutateAsync.mockReset()
    h.testMutateAsync.mockResolvedValue({})
    h.toast.mockClear()
    h.providerData = {
        wire_protocol: "open_ai_chat_completions",
        model: "gpt-4o-mini",
        base_url: "https://api.example/v1",
        max_context_bytes: 131072,
        request_options: {},
        output_limit_field: "max_tokens",
        probe_max_output_tokens: 512,
        runtime_max_output_tokens: 4096,
        supports_image_input: false,
        connection_revision: 1,
        profile_revision: 1,
        response_format: "json_schema",
        execution_mode: "read_only",
        max_steps_per_turn: 20,
        max_same_tool_calls_per_turn: 10,
        exec_approval_timeout_secs: 300,
        api_key_set: true,
    }
})

describe("AiModelSettings", () => {
    it("hydrates the provider form and saves its complete profile while omitting a blank api_key", async () => {
        render(<AiModelSettings />)
        // Hydration populated the form from the masked public view.
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        const payload = lastProviderPayload()
        expect(payload.expected_connection_revision).toBe(1)
        expect(payload.expected_profile_revision).toBe(1)
        expect(payload.response_format).toBe("json_schema")
        expect(payload.wire_protocol).toBe("open_ai_chat_completions")
        expect(payload.base_url).toBe("https://api.example/v1")
        // The central grant carries the execution mode.
        expect(payload.execution_mode).toBe("read_only")
        expect(payload.max_steps_per_turn).toBe(20)
        expect(payload.max_same_tool_calls_per_turn).toBe(10)
        expect(payload.exec_approval_timeout_secs).toBe(300)
        // The provider form no longer carries the collection policy.
        expect(payload.allow_logs).toBeUndefined()
        expect(payload.allow_screen).toBeUndefined()
        // Blank key field → leave unchanged; context capacity is required.
        expect(payload.api_key).toBeUndefined()
        expect(payload.max_context_bytes).toBe(131072)
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

    it("uses the raised 40-round and 20-call defaults when limits are absent", async () => {
        delete h.providerData.max_steps_per_turn
        delete h.providerData.max_same_tool_calls_per_turn
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        expect(lastProviderPayload().max_steps_per_turn).toBe(40)
        expect(lastProviderPayload().max_same_tool_calls_per_turn).toBe(20)
    })

    it("uses the 120-second approval default when the field is absent", async () => {
        delete h.providerData.exec_approval_timeout_secs
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        expect(lastProviderPayload().exec_approval_timeout_secs).toBe(120)
    })

    it("rejects an out-of-range command approval window", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        const timeoutInput = screen.getByLabelText("Command approval window (seconds)")
        fireEvent.change(timeoutInput, {
            target: { value: "29" },
        })
        fireEvent.click(screen.getAllByText("Save Settings")[0])

        expect(timeoutInput).toBeInvalid()
        expect(h.providerMutateAsync).not.toHaveBeenCalled()
    })

    it("defaults a missing execution grant to confirm each action", async () => {
        delete h.providerData.execution_mode
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        expect(lastProviderPayload().execution_mode).toBe("confirm_each_action")
    })

    it("accepts an 80-round limit", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.change(screen.getByLabelText("Model reasoning rounds per turn"), {
            target: { value: "80" },
        })
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())

        expect(lastProviderPayload().max_steps_per_turn).toBe(80)
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
        h.providerData = { ...h.providerData, wire_protocol: "anthropic_messages" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // The provider-specific base URL hint warns against appending /v1.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()

        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalled())
        expect(lastProviderPayload().wire_protocol).toBe("anthropic_messages")
    })

    it("rejects a mixed-case / padded protocol instead of guessing an adapter", async () => {
        h.providerData = { ...h.providerData, wire_protocol: "  Anthropic_Messages  " }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.getAllByText(/Unsupported:/i).length).toBeGreaterThan(0)
        expect(screen.getAllByText("Save Settings")[0]).toBeDisabled()
        expect(screen.getByRole("button", { name: "Test connection" })).toBeDisabled()
    })

    it("rejects an unknown stored protocol instead of silently switching adapters", async () => {
        h.providerData = { ...h.providerData, wire_protocol: "some-legacy-value" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.getAllByText(/Unsupported:/i).length).toBeGreaterThan(0)
        expect(screen.getAllByText("Save Settings")[0]).toBeDisabled()
    })

    it("hides the clear toggle when no key is configured", async () => {
        h.providerData = { ...h.providerData, api_key_set: false, response_format: "json_object" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.queryByRole("switch", { name: /clear stored key/i })).toBeNull()
    })

    it("shows the backend reason when the connection test rejects", async () => {
        const providerDetail = "unknown variant image_url, expected text"
        const reason = `Custom desk error(1): Test failed: model gateway returned status 400 Bad Request: ${providerDetail}`
        h.testMutateAsync.mockRejectedValueOnce(new RestResponseError(reason, 1, null))

        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        fireEvent.click(screen.getByRole("button", { name: "Test connection" }))

        await waitFor(() =>
            expect(h.toast).toHaveBeenCalledWith({
                variant: "destructive",
                title: "Connection test failed",
                description: `Model gateway returned 400 Bad Request: ${providerDetail}`,
            }),
        )
    })

    it("tests the current unsaved form values without saving them", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.change(screen.getByDisplayValue("gpt-4o-mini"), {
            target: { value: "unsaved-vision-model" },
        })
        fireEvent.change(screen.getByDisplayValue("https://api.example/v1"), {
            target: { value: "https://unsaved.example/v1" },
        })
        fireEvent.change(screen.getByPlaceholderText(/Configured/i), {
            target: { value: "unsaved-key" },
        })
        fireEvent.click(screen.getByRole("switch", { name: /supports image input/i }))
        fireEvent.click(screen.getByRole("button", { name: "Test connection" }))

        await waitFor(() =>
            expect(h.testMutateAsync).toHaveBeenCalledWith({
                data: {
                    wire_protocol: "open_ai_chat_completions",
                    model: "unsaved-vision-model",
                    supports_image_input: true,
                    base_url: "https://unsaved.example/v1",
                    request_options: {},
                    output_limit_field: "max_tokens",
                    probe_max_output_tokens: 512,
                    runtime_max_output_tokens: 4096,
                    max_context_bytes: 131072,
                    api_key: "unsaved-key",
                },
            }),
        )
        expect(h.providerMutateAsync).not.toHaveBeenCalled()
    })

    it("materializes the frozen preset budgets and disables the incompatible manual preset", async () => {
        expect(presetProfile("deepseek")).toEqual({
            requestOptions: { thinking: { type: "disabled" } },
            probeBudget: 512,
            runtimeBudget: 4096,
        })
        expect(presetProfile("anthropic_adaptive")).toEqual({
            requestOptions: { thinking: { type: "adaptive", display: "omitted" } },
            probeBudget: 4096,
            runtimeBudget: 8192,
        })
        expect(isProfilePresetSelectable("anthropic_manual")).toBe(false)
        expect(isProfilePresetSelectable("anthropic_adaptive")).toBe(true)

        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.getByText(/terminal completion has a 512-token hard cap/i)).toBeInTheDocument()
    })
})
