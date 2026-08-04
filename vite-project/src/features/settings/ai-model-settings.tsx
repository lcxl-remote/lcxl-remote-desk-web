import { useEffect, useRef, useState } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import type { TFunction } from "i18next"
import { useTranslation } from "react-i18next"
import { Loader2, Save, PlugZap } from "lucide-react"

import { useGetModelProvider } from "@/services/hooks/modelProviderController/useGetModelProvider"
import { useUpdateModelProvider } from "@/services/hooks/modelProviderController/useUpdateModelProvider"
import { useTestModelProvider } from "@/services/hooks/modelProviderController/useTestModelProvider"
import type { ModelProviderUpdate, ProviderTestParams } from "@/services/types"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"
import { RestResponseError } from "@/lib/kubb-client"

const RESPONSE_FORMATS = ["none", "json_object", "json_schema"] as const

// Providers the backend maps to a wire adapter. "openai-compatible" covers
// OpenAI and any OpenAI-compatible gateway; "anthropic" uses the Messages API.
const PROVIDERS = ["openai-compatible", "anthropic"] as const

// The execution modes the confirm-execute flow supports. `session_approved` /
// `automated` are frozen in the protocol but not selectable yet (the backend
// rejects them), so they are intentionally omitted here.
const EXECUTION_MODES = ["suggest_only", "read_only", "confirm_each_action"] as const
const MAX_STEPS_MIN = 1
const MAX_STEPS_MAX = 80
const MAX_STEPS_DEFAULT = 40
const SAME_TOOL_LIMIT_MIN = 1
const SAME_TOOL_LIMIT_MAX = 50
const SAME_TOOL_LIMIT_DEFAULT = 20

const providerSchema = z.object({
    provider: z.enum(PROVIDERS),
    model: z.string(),
    base_url: z.string(),
    api_key: z.string(),
    clear_api_key: z.boolean(),
    supports_image_input: z.boolean(),
    max_context_bytes: z.number().min(0),
    response_format: z.enum(RESPONSE_FORMATS),
    execution_mode: z.enum(EXECUTION_MODES),
    max_steps_per_turn: z
        .number()
        .int()
        .min(MAX_STEPS_MIN)
        .max(MAX_STEPS_MAX),
    max_same_tool_calls_per_turn: z
        .number()
        .int()
        .min(SAME_TOOL_LIMIT_MIN)
        .max(SAME_TOOL_LIMIT_MAX),
})

type ProviderFormValues = z.infer<typeof providerSchema>

function pendingApiKey(values: ProviderFormValues): string | undefined {
    if (values.clear_api_key) return ""
    return values.api_key.trim() === "" ? undefined : values.api_key
}

// Render the central execution-grant choices.
function ExecutionModeItems() {
    const { t } = useTranslation()
    return (
        <>
            <SelectItem value="suggest_only">{t("pages.executionMode.suggestOnly")}</SelectItem>
            <SelectItem value="read_only">{t("pages.executionMode.readOnly")}</SelectItem>
            <SelectItem value="confirm_each_action">{t("pages.executionMode.confirmEachAction")}</SelectItem>
        </>
    )
}

function normalizeExecutionMode(mode: string | undefined): (typeof EXECUTION_MODES)[number] {
    return EXECUTION_MODES.includes(mode as (typeof EXECUTION_MODES)[number])
        ? (mode as (typeof EXECUTION_MODES)[number])
        : "suggest_only"
}

function errorMessage(error: unknown, fallback: string): string {
    return error instanceof Error && error.message ? error.message : fallback
}

function connectionTestErrorMessage(error: unknown, t: TFunction, fallback: string): string {
    if (!(error instanceof RestResponseError)) return errorMessage(error, fallback)

    const message = error.message.replace(/^Custom desk error\(-?\d+\):\s*/, "")
    const gatewayFailure = message.match(
        /^Test failed: model gateway returned status ([^:]+)(?::\s*([\s\S]*))?$/,
    )
    if (gatewayFailure) {
        const [, status, detail] = gatewayFailure
        return detail
            ? t("pages.aiModel.settings.testGatewayFailed", { status, detail })
            : t("pages.aiModel.settings.testGatewayFailedNoDetail", { status })
    }

    const testFailure = message.match(/^Test failed:\s*([\s\S]+)$/)
    if (testFailure) {
        return t("pages.aiModel.settings.testFailedWithReason", { reason: testFailure[1] })
    }
    return message || fallback
}

export function AiModelSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    // Model provider lives on the central signaling brain. In portable mode the
    // signaling server is embedded in this process, so the same origin serves it.
    const { data: providerResponse, isLoading } = useGetModelProvider()
    const { mutateAsync: updateProvider, isPending: isProviderUpdating } = useUpdateModelProvider()
    const { mutateAsync: testProvider, isPending: isTesting } = useTestModelProvider()

    // Whether a key is already stored (the value itself is never returned).
    const [apiKeySet, setApiKeySet] = useState(false)

    const form = useForm<ProviderFormValues>({
        resolver: zodResolver(providerSchema),
        defaultValues: {
            provider: "openai-compatible",
            model: "",
            base_url: "",
            api_key: "",
            clear_api_key: false,
            supports_image_input: false,
            max_context_bytes: 0,
            response_format: "json_object",
            execution_mode: "suggest_only",
            max_steps_per_turn: MAX_STEPS_DEFAULT,
            max_same_tool_calls_per_turn: SAME_TOOL_LIMIT_DEFAULT,
        },
    })

    const didHydrateRef = useRef(false)
    useEffect(() => {
        if (providerResponse?.data && !isLoading && !didHydrateRef.current) {
            didHydrateRef.current = true
            const data = providerResponse.data
            setApiKeySet(data.api_key_set)
            const rf = RESPONSE_FORMATS.includes(data.response_format as (typeof RESPONSE_FORMATS)[number])
                ? (data.response_format as (typeof RESPONSE_FORMATS)[number])
                : "json_object"
            // Normalize like the backend (trim + lowercase) before matching, so a
            // stored "Anthropic" / " anthropic " is recognized rather than being
            // silently switched to openai-compatible. Unknown / legacy values map
            // to openai-compatible (the backend's fallback adapter).
            const provider = (data.provider ?? "").trim().toLowerCase() === "anthropic"
                ? "anthropic"
                : "openai-compatible"
            form.reset({
                provider,
                model: data.model ?? "",
                base_url: data.base_url ?? "",
                api_key: "",
                clear_api_key: false,
                supports_image_input: data.supports_image_input,
                max_context_bytes: data.max_context_bytes ?? 0,
                response_format: rf,
                execution_mode: normalizeExecutionMode(data.execution_mode),
                max_steps_per_turn: data.max_steps_per_turn ?? MAX_STEPS_DEFAULT,
                max_same_tool_calls_per_turn:
                    data.max_same_tool_calls_per_turn ?? SAME_TOOL_LIMIT_DEFAULT,
            })
        }
    }, [providerResponse?.data, isLoading, form])

    const onSubmit = async (values: ProviderFormValues) => {
        if (values.max_steps_per_turn < values.max_same_tool_calls_per_turn) {
            form.setError("max_steps_per_turn", {
                type: "validate",
                message: t("pages.aiModel.settings.maxStepsPerTurn.notBelowSameTool"),
            })
            return
        }
        // api_key is write-only: clearing wins, then a typed value sets it, and
        // an empty field leaves the stored key unchanged (omit it).
        const api_key = pendingApiKey(values)

        const payload: ModelProviderUpdate = {
            provider: values.provider,
            model: values.model,
            base_url: values.base_url,
            // 0 means "use the default budget" — leave the stored value unchanged.
            max_context_bytes: values.max_context_bytes > 0 ? values.max_context_bytes : undefined,
            response_format: values.response_format,
            execution_mode: values.execution_mode,
            max_steps_per_turn: values.max_steps_per_turn,
            max_same_tool_calls_per_turn: values.max_same_tool_calls_per_turn,
            supports_image_input: values.supports_image_input,
            api_key,
        }

        try {
            await updateProvider({ data: payload })
            // Reflect the new key state and reset the transient secret inputs.
            if (values.clear_api_key) setApiKeySet(false)
            else if (api_key) setApiKeySet(true)
            form.setValue("api_key", "")
            form.setValue("clear_api_key", false)
            toast({
                title: t("pages.system.settings.success"),
                description: t("pages.aiModel.settings.updateSucceedMessage"),
            })
        } catch {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error"),
                description: t("pages.aiModel.settings.updateFailedMessage"),
            })
        }
    }

    // Probe the form's current provider fields end-to-end without saving them.
    // A blank key reuses the stored secret; a typed key is transient. The shared
    // HTTP client turns a RestResponse business failure into an Error, whose
    // backend-provided reason remains visible to the operator.
    const onTestConnection = async () => {
        try {
            const values = form.getValues()
            const payload: ProviderTestParams = {
                provider: values.provider,
                model: values.model,
                supports_image_input: values.supports_image_input,
                base_url: values.base_url,
                api_key: pendingApiKey(values),
            }
            const res = await testProvider({ data: payload })
            if (res?.success === false) {
                toast({
                    variant: "destructive",
                    title: t("pages.aiModel.settings.testFailed"),
                    description: res.message ?? t("pages.aiModel.settings.testFailed"),
                })
                return
            }
            const latency = res?.data?.latency_ms
            const sample = res?.data?.sample
            const validatedCapabilities = res?.data?.validated_capabilities ?? []
            toast({
                title: validatedCapabilities.includes("image_input")
                    ? t("pages.aiModel.settings.testImageSucceed")
                    : t("pages.aiModel.settings.testSucceed"),
                description: sample
                    ? t("pages.aiModel.settings.testResult", { latency, sample })
                    : t("pages.aiModel.settings.testResultNoSample", { latency }),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.aiModel.settings.testFailed"),
                description: connectionTestErrorMessage(
                    error,
                    t,
                    t("pages.aiModel.settings.testFailed"),
                ),
            })
        }
    }

    if (isLoading) {
        return (
            <div className="flex h-full items-center justify-center">
                <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
            </div>
        )
    }

    return (
        <div className="container mx-auto max-w-4xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t("pages.aiModel.settings.title")}</h1>
                <p className="text-muted-foreground">
                    {t("pages.aiModel.settings.description")}
                </p>
                <p className="mt-2 text-sm text-amber-600 dark:text-amber-500">
                    {t("pages.aiModel.settings.tlsHint")}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.aiModel.settings.gateway")}</CardTitle>
                    <CardDescription>
                        {t("pages.aiModel.settings.gateway.description")}
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="provider"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.provider")}</FormLabel>
                                            <Select
                                                key={field.value || "provider-empty"}
                                                onValueChange={field.onChange}
                                                defaultValue={field.value}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="openai-compatible">{t("pages.aiModel.settings.provider.openaiCompatible")}</SelectItem>
                                                    <SelectItem value="anthropic">{t("pages.aiModel.settings.provider.anthropic")}</SelectItem>
                                                </SelectContent>
                                            </Select>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.provider.description")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                                <FormField
                                    control={form.control}
                                    name="model"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.model")}</FormLabel>
                                            <FormControl>
                                                <Input placeholder="gpt-4o-mini" {...field} />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <FormField
                                control={form.control}
                                name="base_url"
                                render={({ field }) => {
                                    const isAnthropic = form.watch("provider") === "anthropic"
                                    return (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.baseUrl")}</FormLabel>
                                            <FormControl>
                                                <Input
                                                    placeholder={isAnthropic ? "https://api.anthropic.com" : "https://api.openai.com/v1"}
                                                    {...field}
                                                />
                                            </FormControl>
                                            <FormDescription>
                                                {isAnthropic
                                                    ? t("pages.aiModel.settings.baseUrl.anthropic")
                                                    : t("pages.aiModel.settings.baseUrl.openai")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )
                                }}
                            />

                            <FormField
                                control={form.control}
                                name="api_key"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.aiModel.settings.apiKey")}</FormLabel>
                                        <FormControl>
                                            <Input
                                                type="password"
                                                autoComplete="off"
                                                placeholder={
                                                    apiKeySet
                                                        ? t("pages.aiModel.settings.apiKeySet")
                                                        : t("pages.aiModel.settings.apiKeyUnset")
                                                }
                                                disabled={form.watch("clear_api_key")}
                                                {...field}
                                            />
                                        </FormControl>
                                        <FormDescription>
                                            {t("pages.aiModel.settings.apiKey.description")}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            {apiKeySet && (
                                <FormField
                                    control={form.control}
                                    name="clear_api_key"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg border p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.aiModel.settings.clearApiKey")}</FormLabel>
                                                <FormDescription>
                                                    {t("pages.aiModel.settings.clearApiKey.description")}
                                                </FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                            )}

                            <FormField
                                control={form.control}
                                name="supports_image_input"
                                render={({ field }) => (
                                    <FormItem className="flex flex-row items-center justify-between rounded-lg border p-3 shadow-sm">
                                        <div className="space-y-0.5">
                                            <FormLabel>
                                                {t("pages.aiModel.settings.supportsImageInput")}
                                            </FormLabel>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.supportsImageInput.description")}
                                            </FormDescription>
                                        </div>
                                        <FormControl>
                                            <Switch
                                                checked={field.value}
                                                onCheckedChange={field.onChange}
                                            />
                                        </FormControl>
                                    </FormItem>
                                )}
                            />

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="response_format"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.responseFormat")}</FormLabel>
                                            <Select
                                                key={field.value || "response-format-empty"}
                                                onValueChange={field.onChange}
                                                defaultValue={field.value}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="none">{t("pages.aiModel.settings.responseFormat.none")}</SelectItem>
                                                    <SelectItem value="json_object">{t("pages.aiModel.settings.responseFormat.jsonObject")}</SelectItem>
                                                    <SelectItem value="json_schema">{t("pages.aiModel.settings.responseFormat.jsonSchema")}</SelectItem>
                                                </SelectContent>
                                            </Select>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.responseFormat.description")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                                <FormField
                                    control={form.control}
                                    name="max_context_bytes"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.maxContextBytes")}</FormLabel>
                                            <FormControl>
                                                <Input
                                                    key={`max-context-${field.value ?? "empty"}`}
                                                    type="number"
                                                    {...field}
                                                    value={field.value ?? ""}
                                                    onChange={e => field.onChange(e.target.value === "" ? 0 : Number(e.target.value))}
                                                />
                                            </FormControl>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.maxContextBytes.description")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <FormField
                                control={form.control}
                                name="execution_mode"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t("pages.aiModel.settings.executionMode")}</FormLabel>
                                        <Select
                                            key={field.value || "execution-mode-empty"}
                                            onValueChange={field.onChange}
                                            defaultValue={field.value}
                                        >
                                            <FormControl>
                                                <SelectTrigger>
                                                    <SelectValue />
                                                </SelectTrigger>
                                            </FormControl>
                                            <SelectContent>
                                                <ExecutionModeItems />
                                            </SelectContent>
                                        </Select>
                                        <FormDescription>
                                            {t("pages.aiModel.settings.executionMode.description")}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <FormField
                                control={form.control}
                                name="max_steps_per_turn"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>
                                            {t("pages.aiModel.settings.maxStepsPerTurn")}
                                        </FormLabel>
                                        <FormControl>
                                            <Input
                                                type="number"
                                                min={MAX_STEPS_MIN}
                                                max={MAX_STEPS_MAX}
                                                step={1}
                                                {...field}
                                                value={field.value}
                                                onChange={(event) =>
                                                    field.onChange(Number(event.target.value))
                                                }
                                            />
                                        </FormControl>
                                        <FormDescription>
                                            {t(
                                                "pages.aiModel.settings.maxStepsPerTurn.description",
                                                {
                                                    min: MAX_STEPS_MIN,
                                                    max: MAX_STEPS_MAX,
                                                    defaultValue: MAX_STEPS_DEFAULT,
                                                },
                                            )}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <FormField
                                control={form.control}
                                name="max_same_tool_calls_per_turn"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>
                                            {t("pages.aiModel.settings.maxSameToolCallsPerTurn")}
                                        </FormLabel>
                                        <FormControl>
                                            <Input
                                                type="number"
                                                min={SAME_TOOL_LIMIT_MIN}
                                                max={SAME_TOOL_LIMIT_MAX}
                                                step={1}
                                                {...field}
                                                value={field.value}
                                                onChange={(event) =>
                                                    field.onChange(Number(event.target.value))
                                                }
                                            />
                                        </FormControl>
                                        <FormDescription>
                                            {t(
                                                "pages.aiModel.settings.maxSameToolCallsPerTurn.description",
                                                {
                                                    min: SAME_TOOL_LIMIT_MIN,
                                                    max: SAME_TOOL_LIMIT_MAX,
                                                    defaultValue: SAME_TOOL_LIMIT_DEFAULT,
                                                },
                                            )}
                                        </FormDescription>
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            <div className="flex justify-end gap-2">
                                <Button type="button" variant="outline" onClick={onTestConnection} disabled={isTesting || isProviderUpdating}>
                                    {isTesting ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <PlugZap className="mr-2 h-4 w-4" />}
                                    {t("pages.aiModel.settings.testConnection")}
                                </Button>
                                <Button type="submit" disabled={isProviderUpdating}>
                                    {isProviderUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t("pages.system.settings.save")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>

        </div>
    )
}
