import { describe, it, expect, vi, beforeEach } from "vitest"
import { render, screen, fireEvent, waitFor } from "@testing-library/react"

// i18n: t() echoes the fallback the component always provides.
vi.mock("react-i18next", () => ({
    useTranslation: () => ({
        t: (_key: string, fallback?: string) => fallback ?? _key,
    }),
}))

vi.mock("@/hooks/use-toast", () => ({
    useToast: () => ({ toast: () => undefined }),
}))

// Mutable query payload + spy on the update mutation.
const h = vi.hoisted(() => ({
    publicData: {} as Record<string, unknown>,
    mutateAsync: vi.fn(async () => ({})),
}))

vi.mock("@/services/hooks/aiModelController/useQueryAiModelSettings", () => ({
    useQueryAiModelSettings: () => ({ data: { data: h.publicData }, isLoading: false }),
}))
vi.mock("@/services/hooks/aiModelController/useUpdateAiModelSettings", () => ({
    useUpdateAiModelSettings: () => ({ mutateAsync: h.mutateAsync, isPending: false }),
}))

import { AiModelSettings } from "./ai-model-settings"

function lastPayload() {
    return (h.mutateAsync.mock.calls[0][0] as { data: Record<string, unknown> }).data
}

beforeEach(() => {
    h.mutateAsync.mockClear()
    h.publicData = {
        provider: "openai-compatible",
        model: "gpt-4o-mini",
        base_url: "https://api.example/v1",
        allow_screen: false,
        allow_logs: true,
        max_context_bytes: 0,
        response_format: "json_schema",
        api_key_set: true,
    }
})

describe("AiModelSettings", () => {
    it("hydrates from the query and saves the hydrated response_format, omitting a blank api_key and a zero budget", async () => {
        render(<AiModelSettings />)
        // Hydration populated the form from the masked public view.
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())

        fireEvent.click(screen.getByText("Save Settings"))
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())

        const payload = lastPayload()
        expect(payload.response_format).toBe("json_schema")
        expect(payload.provider).toBe("openai-compatible")
        expect(payload.base_url).toBe("https://api.example/v1")
        expect(payload.allow_logs).toBe(true)
        // Blank key field → leave unchanged (omitted); 0 budget → default (omitted).
        expect(payload.api_key).toBeUndefined()
        expect(payload.max_context_bytes).toBeUndefined()
    })

    it("includes a typed api_key in the payload", async () => {
        render(<AiModelSettings />)
        const keyInput = await screen.findByPlaceholderText(/Configured/i)
        fireEvent.change(keyInput, { target: { value: "sk-typed" } })

        fireEvent.click(screen.getByText("Save Settings"))
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().api_key).toBe("sk-typed")
    })

    it("the clear toggle sends an empty api_key (clear semantics)", async () => {
        render(<AiModelSettings />)
        // The clear switch only renders when a key is already configured.
        const clearSwitch = await screen.findByRole("switch", { name: /clear stored key/i })
        fireEvent.click(clearSwitch)

        fireEvent.click(screen.getByText("Save Settings"))
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().api_key).toBe("")
    })

    it("hydrates the anthropic provider, shows its base-URL hint, and saves it", async () => {
        h.publicData = { ...h.publicData, provider: "anthropic" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        // The provider-specific base URL hint warns against appending /v1.
        expect(screen.getByText(/Host root only/i)).toBeInTheDocument()

        fireEvent.click(screen.getByText("Save Settings"))
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
        fireEvent.click(screen.getByText("Save Settings"))
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().provider).toBe("anthropic")
    })

    it("normalizes an unknown stored provider to openai-compatible", async () => {
        h.publicData = { ...h.publicData, provider: "some-legacy-value" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        fireEvent.click(screen.getByText("Save Settings"))
        await waitFor(() => expect(h.mutateAsync).toHaveBeenCalled())
        expect(lastPayload().provider).toBe("openai-compatible")
    })

    it("hides the clear toggle when no key is configured", async () => {
        h.publicData = { ...h.publicData, api_key_set: false, response_format: "json_object" }
        render(<AiModelSettings />)
        await waitFor(() => expect(screen.getByDisplayValue("gpt-4o-mini")).toBeInTheDocument())
        expect(screen.queryByRole("switch", { name: /clear stored key/i })).toBeNull()
    })
})
