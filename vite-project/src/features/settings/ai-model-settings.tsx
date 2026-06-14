import { useEffect, useRef, useState } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQueryAiModelSettings } from "@/services/hooks/aiModelController/useQueryAiModelSettings"
import { useUpdateAiModelSettings } from "@/services/hooks/aiModelController/useUpdateAiModelSettings"
import type { AiModelSettingsUpdate } from "@/services/types"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"

const RESPONSE_FORMATS = ["none", "json_object", "json_schema"] as const

// Providers the backend maps to a wire adapter. "openai-compatible" covers
// OpenAI and any OpenAI-compatible gateway; "anthropic" uses the Messages API.
const PROVIDERS = ["openai-compatible", "anthropic"] as const

const aiModelSettingsSchema = z.object({
    provider: z.enum(PROVIDERS),
    model: z.string(),
    base_url: z.string(),
    api_key: z.string(),
    clear_api_key: z.boolean(),
    allow_screen: z.boolean(),
    allow_logs: z.boolean(),
    max_context_bytes: z.number().min(0),
    response_format: z.enum(RESPONSE_FORMATS),
})

type AiModelSettingsFormValues = z.infer<typeof aiModelSettingsSchema>

export function AiModelSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQueryAiModelSettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateAiModelSettings()

    // Whether a key is already stored (the value itself is never returned).
    const [apiKeySet, setApiKeySet] = useState(false)

    const form = useForm<AiModelSettingsFormValues>({
        resolver: zodResolver(aiModelSettingsSchema),
        defaultValues: {
            provider: "openai-compatible",
            model: "",
            base_url: "",
            api_key: "",
            clear_api_key: false,
            allow_screen: false,
            allow_logs: false,
            max_context_bytes: 0,
            response_format: "json_object",
        },
    })

    const didHydrateRef = useRef(false)
    useEffect(() => {
        if (settingsResponse?.data && !isLoading && !didHydrateRef.current) {
            didHydrateRef.current = true
            const data = settingsResponse.data
            setApiKeySet(data.api_key_set)
            const rf = RESPONSE_FORMATS.includes(data.response_format as (typeof RESPONSE_FORMATS)[number])
                ? (data.response_format as (typeof RESPONSE_FORMATS)[number])
                : "json_object"
            // Normalize any stored value to a known provider; unknown / legacy
            // values map to openai-compatible (the backend's fallback adapter).
            const provider = data.provider === "anthropic" ? "anthropic" : "openai-compatible"
            form.reset({
                provider,
                model: data.model ?? "",
                base_url: data.base_url ?? "",
                api_key: "",
                clear_api_key: false,
                allow_screen: data.allow_screen ?? false,
                allow_logs: data.allow_logs ?? false,
                max_context_bytes: data.max_context_bytes ?? 0,
                response_format: rf,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: AiModelSettingsFormValues) => {
        // api_key is write-only: clearing wins, then a typed value sets it, and
        // an empty field leaves the stored key unchanged (omit it).
        let api_key: string | undefined
        if (values.clear_api_key) {
            api_key = ""
        } else if (values.api_key.trim() !== "") {
            api_key = values.api_key
        }

        const payload: AiModelSettingsUpdate = {
            provider: values.provider,
            model: values.model,
            base_url: values.base_url,
            allow_screen: values.allow_screen,
            allow_logs: values.allow_logs,
            // 0 means "use the default budget" — leave the stored value unchanged.
            max_context_bytes: values.max_context_bytes > 0 ? values.max_context_bytes : undefined,
            response_format: values.response_format,
            api_key,
        }

        try {
            await updateSettings({ data: payload })
            // Reflect the new key state and reset the transient secret inputs.
            if (values.clear_api_key) setApiKeySet(false)
            else if (api_key) setApiKeySet(true)
            form.setValue("api_key", "")
            form.setValue("clear_api_key", false)
            toast({
                title: t("pages.system.settings.success", "Success"),
                description: t("pages.aiModel.settings.updateSucceedMessage", "AI model settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error", "Error"),
                description: t("pages.aiModel.settings.updateFailedMessage", "Failed to update AI model settings"),
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
                <h1 className="text-3xl font-bold tracking-tight">{t("pages.aiModel.settings.title", "AI Model")}</h1>
                <p className="text-muted-foreground">
                    {t("pages.aiModel.settings.description", "Configure the AI model gateway used for diagnosis.")}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.aiModel.settings.gateway", "Model Gateway")}</CardTitle>
                    <CardDescription>
                        {t("pages.aiModel.settings.gateway.description", "OpenAI-compatible chat-completions endpoint.")}
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
                                            <FormLabel>{t("pages.aiModel.settings.provider", "Provider")}</FormLabel>
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
                                                    <SelectItem value="openai-compatible">{t("pages.aiModel.settings.provider.openaiCompatible", "OpenAI-compatible")}</SelectItem>
                                                    <SelectItem value="anthropic">{t("pages.aiModel.settings.provider.anthropic", "Anthropic")}</SelectItem>
                                                </SelectContent>
                                            </Select>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.provider.description", "Selects the wire protocol (adapter). OpenAI-compatible uses /chat/completions; Anthropic uses the Messages API.")}
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
                                            <FormLabel>{t("pages.aiModel.settings.model", "Model")}</FormLabel>
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
                                            <FormLabel>{t("pages.aiModel.settings.baseUrl", "Base URL")}</FormLabel>
                                            <FormControl>
                                                <Input
                                                    placeholder={isAnthropic ? "https://api.anthropic.com" : "https://api.openai.com/v1"}
                                                    {...field}
                                                />
                                            </FormControl>
                                            <FormDescription>
                                                {isAnthropic
                                                    ? t("pages.aiModel.settings.baseUrl.anthropic", "Host root only (e.g. https://api.anthropic.com); the /v1/messages path is appended automatically.")
                                                    : t("pages.aiModel.settings.baseUrl.openai", "Include the version path (e.g. https://api.openai.com/v1); /chat/completions is appended automatically.")}
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
                                        <FormLabel>{t("pages.aiModel.settings.apiKey", "API Key")}</FormLabel>
                                        <FormControl>
                                            <Input
                                                type="password"
                                                autoComplete="off"
                                                placeholder={
                                                    apiKeySet
                                                        ? t("pages.aiModel.settings.apiKeySet", "Configured — leave blank to keep")
                                                        : t("pages.aiModel.settings.apiKeyUnset", "Not configured")
                                                }
                                                disabled={form.watch("clear_api_key")}
                                                {...field}
                                            />
                                        </FormControl>
                                        <FormDescription>
                                            {t("pages.aiModel.settings.apiKey.description", "Write-only. Stored server-side and never returned.")}
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
                                                <FormLabel>{t("pages.aiModel.settings.clearApiKey", "Clear stored key")}</FormLabel>
                                                <FormDescription>
                                                    {t("pages.aiModel.settings.clearApiKey.description", "Remove the stored API key on save.")}
                                                </FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                            )}

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="response_format"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.aiModel.settings.responseFormat", "Output Format")}</FormLabel>
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
                                                    <SelectItem value="none">{t("pages.aiModel.settings.responseFormat.none", "None (free text)")}</SelectItem>
                                                    <SelectItem value="json_object">{t("pages.aiModel.settings.responseFormat.jsonObject", "JSON object")}</SelectItem>
                                                    <SelectItem value="json_schema">{t("pages.aiModel.settings.responseFormat.jsonSchema", "JSON schema (strict)")}</SelectItem>
                                                </SelectContent>
                                            </Select>
                                            <FormDescription>
                                                {t("pages.aiModel.settings.responseFormat.description", "How the gateway is asked to constrain output. json_schema only helps if the gateway enforces it.")}
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
                                            <FormLabel>{t("pages.aiModel.settings.maxContextBytes", "Max Context (bytes)")}</FormLabel>
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
                                                {t("pages.aiModel.settings.maxContextBytes.description", "0 uses the default budget (128 KB).")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="space-y-4 rounded-md border p-4">
                                <h3 className="text-sm font-medium">{t("pages.aiModel.settings.evidence", "Evidence sent to the model")}</h3>
                                <FormField
                                    control={form.control}
                                    name="allow_logs"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.aiModel.settings.allowLogs", "Allow logs")}</FormLabel>
                                                <FormDescription>
                                                    {t("pages.aiModel.settings.allowLogs.description", "Include recent logs / container logs in the evidence (redacted).")}
                                                </FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                                <FormField
                                    control={form.control}
                                    name="allow_screen"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.aiModel.settings.allowScreen", "Allow screenshot")}</FormLabel>
                                                <FormDescription>
                                                    {t("pages.aiModel.settings.allowScreen.description", "Allow attaching a screenshot when the user opts in.")}
                                                </FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="flex justify-end">
                                <Button type="submit" disabled={isUpdating}>
                                    {isUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t("pages.system.settings.save", "Save Settings")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
