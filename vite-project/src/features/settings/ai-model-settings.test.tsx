import { describe, it, expect, vi, beforeEach } from "vitest"
import { render as renderComponent, screen, fireEvent, waitFor, act } from "@testing-library/react"
import type { ReactElement } from "react"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"

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

vi.mock("@/services/hooks/modelProviderController/useGetModelProvider", async () => {
    const { useQuery } = await import("@tanstack/react-query")
    const getModelProviderQueryKey = () => [{ url: '/api/model/provider' }] as const
    return {
        getModelProviderQueryKey,
        useGetModelProvider: () => useQuery({
            queryKey: getModelProviderQueryKey(),
            queryFn: async () => ({ success: true, data: h.providerData }),
            staleTime: Infinity,
        }),
    }
})
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
import { deskErrorCodeEnum } from "@/services/types"

let queryClient: QueryClient
function render(component: ReactElement) {
    return renderComponent(component, {
        wrapper: ({ children }) => <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>,
    })
}

function lastProviderPayload() {
    return (h.providerMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
}

beforeEach(() => {
    queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    h.providerMutateAsync.mockReset()
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
    h.providerMutateAsync.mockResolvedValue({ success: true, data: h.providerData })
})

describe("AiModelSettings", () => {
    it("uses the saved revisions on subsequent saves without refetching or losing edits", async () => {
        h.providerMutateAsync.mockResolvedValueOnce({
            success: true,
            data: { ...h.providerData, connection_revision: 2, profile_revision: 3 },
        })
        render(<AiModelSettings />)
        await screen.findByDisplayValue("gpt-4o-mini")
        fireEvent.change(screen.getByDisplayValue("gpt-4o-mini"), { target: { value: "edited-model" } })
        fireEvent.click(screen.getByRole("button", { name: "Save Settings" }))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({ title: "Success" })))
        expect(lastProviderPayload()).toMatchObject({ expected_connection_revision: 1, expected_profile_revision: 1 })
        expect(queryClient.getQueryData([{ url: '/api/model/provider' }])).toMatchObject({
            data: { connection_revision: 2, profile_revision: 3 },
        })
        expect(screen.getByDisplayValue("edited-model")).toBeInTheDocument()
        fireEvent.change(screen.getByLabelText("Runtime output tokens"), { target: { value: "8192" } })
        fireEvent.click(screen.getByRole("button", { name: "Save Settings" }))
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalledTimes(2))
        expect(h.providerMutateAsync.mock.calls[1][0]).toMatchObject({
            data: { expected_connection_revision: 2, expected_profile_revision: 3, model: "edited-model", runtime_max_output_tokens: 8192 },
        })
    })

    it("explains a real revision conflict and preserves the unsaved form", async () => {
        h.providerMutateAsync.mockRejectedValueOnce(new RestResponseError(
            "Custom desk error(8): provider configuration revision conflict", deskErrorCodeEnum.PRECONDITION_FAILED, null,
        ))
        render(<AiModelSettings />)
        await screen.findByDisplayValue("gpt-4o-mini")
        fireEvent.change(screen.getByDisplayValue("gpt-4o-mini"), { target: { value: "unsaved-model" } })
        fireEvent.click(screen.getByRole("button", { name: "Save Settings" }))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith({
            variant: "destructive",
            title: "Error",
            description: "The model configuration was updated elsewhere. Refresh this page, review the latest settings, and save again.",
        }))
        expect(screen.getByDisplayValue("unsaved-model")).toBeInTheDocument()
        expect(h.providerMutateAsync).toHaveBeenCalledOnce()
        expect(queryClient.getQueryData([{ url: '/api/model/provider' }])).toMatchObject({
            data: { connection_revision: 1, profile_revision: 1 },
        })
    })

    it("shows other backend save errors instead of only the generic failure", async () => {
        h.providerMutateAsync.mockRejectedValueOnce(new RestResponseError(
            "Custom desk error(2): base_url is not permitted by the server's provider policy", deskErrorCodeEnum.INVALID_PARAMS, null,
        ))
        render(<AiModelSettings />)
        await screen.findByDisplayValue("gpt-4o-mini")
        fireEvent.click(screen.getByRole("button", { name: "Save Settings" }))
        await waitFor(() => expect(h.toast).toHaveBeenCalledWith(expect.objectContaining({
            variant: "destructive",
            description: "base_url is not permitted by the server's provider policy",
        })))
    })

    it("prevents another save or probe while the previous save is completing", async () => {
        let finish!: (value: unknown) => void
        h.providerMutateAsync.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        render(<AiModelSettings />)
        await screen.findByDisplayValue("gpt-4o-mini")
        const save = screen.getByRole("button", { name: "Save Settings" })
        fireEvent.click(save)
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalledOnce())
        expect(save).toBeDisabled()
        expect(screen.getByRole("button", { name: "Test connection" })).toBeDisabled()
        fireEvent.click(save)
        expect(h.providerMutateAsync).toHaveBeenCalledOnce()
        await act(async () => finish({ success: true, data: h.providerData }))
        await waitFor(() => expect(save).not.toBeDisabled())
    })

    it("uses the edited runtime budget for connection tests and saved settings", async () => {
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.queryByLabelText("Probe output tokens")).toBeNull()
        fireEvent.change(screen.getByLabelText("Runtime output tokens"), { target: { value: "8192" } })
        fireEvent.click(screen.getByRole("button", { name: "Test connection" }))
        await waitFor(() => expect(h.testMutateAsync).toHaveBeenCalledOnce())
        const payload = (h.testMutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
        expect(payload.runtime_max_output_tokens).toBe(8192)
        expect(payload).not.toHaveProperty("probe_max_output_tokens")
        fireEvent.click(screen.getAllByText("Save Settings")[0])
        await waitFor(() => expect(h.providerMutateAsync).toHaveBeenCalledOnce())
        expect(lastProviderPayload().runtime_max_output_tokens).toBe(8192)
        expect(lastProviderPayload()).not.toHaveProperty("probe_max_output_tokens")
    })

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
        expect(screen.getByLabelText("Runtime output tokens")).toHaveValue(4096)

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
                    reasoning_contract: 'conservative',
                    anthropic_prefix_binding: false,
                    request_options: {},
                    output_limit_field: "max_tokens",
                    runtime_max_output_tokens: 4096,
                    max_context_bytes: 131072,
                    api_key: "unsaved-key",
                },
            }),
        )
        expect(h.providerMutateAsync).not.toHaveBeenCalled()
    })

    it("materializes the preset budgets and allows configured manual thinking", async () => {
        expect(presetProfile("standard")).toEqual({ requestOptions: {}, runtimeBudget: 65536 })
        expect(presetProfile("deepseek")).toEqual({
            requestOptions: { thinking: { type: "disabled" } },
            runtimeBudget: 65536,
        })
        expect(presetProfile("anthropic_adaptive")).toEqual({
            requestOptions: { thinking: { type: "adaptive", display: "omitted" } },
            runtimeBudget: 65536,
        })
        expect(isProfilePresetSelectable("anthropic_manual")).toBe(true)
        expect(isProfilePresetSelectable("anthropic_adaptive")).toBe(true)

        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.getByText(/thinking budget must be smaller than the effective output limit/i)).toBeInTheDocument()
    })
})
