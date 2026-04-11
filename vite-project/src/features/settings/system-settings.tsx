import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/undefinedController/useQuerySettings"
import { useUpdateSettings } from "@/services/hooks/undefinedController/useUpdateSettings"
import { useQueryServerInfo } from "@/services/hooks/undefinedController/useQueryServerInfo"
import { useQueryBackendInfo } from "@/services/hooks/undefinedController/useQueryBackendInfo"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useToast } from "@/hooks/use-toast"
import { TelemetryDisclosure } from "@/components/telemetry-disclosure"

const systemSettingsSchema = z.object({
    enable_ipv6: z.boolean(),
    telemetry_consent: z.boolean().nullable(),
    auto_start: z.boolean().nullable(),
    listen_addr_ipv4: z.string().min(1, "IPv4 address is required"),
    listen_addr_ipv6: z.string(),
    signaling_url: z.string().nullable(),
    signaling_token: z.string().nullable(),
    manager_url: z.string().nullable(),
    manager_api_token: z.string().nullable(),
    port: z.number().min(1).max(65535),
})

type SystemSettingsFormValues = z.infer<typeof systemSettingsSchema>

export function SystemSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQuerySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSettings()
    const { data: serverInfoResp } = useQueryServerInfo()
    const { data: backendInfoResp } = useQueryBackendInfo()

    const serverInfo = serverInfoResp?.data
    const backendInfo = backendInfoResp?.data

    const form = useForm<SystemSettingsFormValues>({
        resolver: zodResolver(systemSettingsSchema),
        defaultValues: {
            enable_ipv6: true,
            telemetry_consent: null,
            auto_start: null,
            listen_addr_ipv4: "0.0.0.0",
            listen_addr_ipv6: "::",
            signaling_url: null,
            signaling_token: null,
            manager_url: null,
            manager_api_token: null,
            port: 8081,
        },
    })

    // Update form values once data is loaded
    useEffect(() => {
        if (settingsResponse?.data && !form.formState.isDirty && !isLoading) {
            const data = settingsResponse.data
            form.reset({
                enable_ipv6: data.enable_ipv6 ?? true,
                telemetry_consent: data.telemetry_consent ?? null,
                auto_start: data.auto_start ?? null,
                listen_addr_ipv4: data.listen_addr_ipv4 || "0.0.0.0",
                listen_addr_ipv6: data.listen_addr_ipv6 || "::",
                signaling_url: data.signaling_url || null,
                signaling_token: data.signaling_token || null,
                manager_url: data.manager_url || null,
                manager_api_token: data.manager_api_token || null,
                port: data.port || 8081,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: SystemSettingsFormValues) => {
        try {
            await updateSettings({ data: values })
            toast({
                title: t('pages.system.settings.success', 'Success'),
                description: t('pages.system.settings.updateSucceedMessage', "System settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error', 'Error'),
                description: t('pages.system.settings.updateFailedMessage', "Failed to update system settings"),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.system.settings.title', 'System Settings')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.system.settings.description', 'Manage global device configuration and server settings')}
                </p>
            </div>

            <Alert variant="default" className="mb-6 border-amber-500/50 bg-amber-500/10 text-amber-600 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-500">
                <AlertTitle>{t("pages.system.settings.alert.message", "Warning")}</AlertTitle>
                <AlertDescription>
                    {t("pages.system.settings.alert.description", "Modifying these settings may affect remote connections and require a restart to take full effect.")}
                </AlertDescription>
            </Alert>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.system.settings.configuration", "Configuration")}</CardTitle>
                    <CardDescription>{t("pages.system.settings.configuration.description", "Update the server properties.")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="port"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.port", "Listen Port")}</FormLabel>
                                            <FormControl>
                                                <Input type="number" {...field} onChange={e => field.onChange(e.target.value === '' ? 0 : Number(e.target.value))} />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="listen_addr_ipv4"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.listenAddrIpv4", "IPv4 Listen Address")}</FormLabel>
                                            <FormControl>
                                                <Input {...field} placeholder="0.0.0.0" />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="listen_addr_ipv6"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.listenAddrIpv6", "IPv6 Listen Address")}</FormLabel>
                                            <FormControl>
                                                <Input {...field} placeholder="::" />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                {serverInfo?.startup_mode !== "signaling" && (
                                    <>
                                        <FormField
                                            control={form.control}
                                            name="signaling_url"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.signalingUrl", "Signaling Server URL")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="ws://127.0.0.1:8081/api/desk/signaling" />
                                                    </FormControl>
                                                    <FormDescription>{t("pages.system.settings.signalingUrl.description", "Leave blank to use the default internal signaling server.")}</FormDescription>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="signaling_token"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.signalingToken", "Signaling Access Token")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="Node access token for remote signaling..." />
                                                    </FormControl>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="manager_url"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.managerUrl", "Manager Server URL")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="ws://manager.example.com/api/desk/signaling" />
                                                    </FormControl>
                                                    <FormDescription>{t("pages.system.settings.managerUrl.description", "If using a central manager server, enter its signaling URL here.")}</FormDescription>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="manager_api_token"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.managerApiToken", "Manager API Token")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="Access token for the manager server..." />
                                                    </FormControl>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                    </>
                                )}
                            </div>

                            <div className="space-y-4 rounded-md border p-4">

                                <FormField
                                    control={form.control}
                                    name="enable_ipv6"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.enableIpv6", "Enable IPv6")}</FormLabel>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="telemetry_consent"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel className="flex items-center gap-2">
                                                    {t("pages.system.settings.telemetry_consent", "Telemetry Consent")}
                                                    <TelemetryDisclosure />
                                                </FormLabel>
                                                <FormDescription>{t("pages.system.settings.telemetry_consent.tooltip", "Help improve our product by sending anonymous usage data.")}</FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value ?? false} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="auto_start"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.auto_start", "Auto-Start at Login")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.auto_start.tooltip", "Automatically start the application in the background when you log in to the OS.")}</FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value ?? false} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="flex justify-end">
                                <Button type="submit" disabled={isUpdating}>
                                    {isUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t('pages.system.settings.save', 'Save Settings')}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>

            <Card className="mt-6">
                <CardHeader>
                    <CardTitle>{t("pages.system.settings.backendDiagnostics", "Backend Diagnostics")}</CardTitle>
                    <CardDescription>{t("pages.system.settings.backendDiagnostics.description", "Wayland/X11 capture and control runtime status.")}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-2 text-sm">
                    <div><span className="font-medium">OS:</span> {backendInfo?.os ?? "-"}</div>
                    <div><span className="font-medium">WAYLAND_DISPLAY:</span> {String(backendInfo?.wayland_env ?? false)}</div>
                    <div><span className="font-medium">DISPLAY:</span> {String(backendInfo?.x11_env ?? false)}</div>
                    <div><span className="font-medium">Capture Backend:</span> {backendInfo?.resolved_image_capture ?? "-"}</div>
                    <div><span className="font-medium">Input Backend:</span> {backendInfo?.resolved_input_control ?? "-"}</div>
                    <div><span className="font-medium">Input Runtime:</span> {backendInfo?.input_backend_runtime_status ?? "-"}</div>
                    <div><span className="font-medium">Portal Available:</span> {backendInfo?.portal_available === undefined ? "-" : String(backendInfo.portal_available)}</div>
                    {backendInfo?.input_backend_error && (
                        <Alert variant="destructive" className="mt-2">
                            <AlertTitle>{t("pages.system.settings.backendDiagnostics.inputError", "Input Backend Error")}</AlertTitle>
                            <AlertDescription>{backendInfo.input_backend_error}</AlertDescription>
                        </Alert>
                    )}
                    {backendInfo?.portal_error && (
                        <Alert variant="destructive" className="mt-2">
                            <AlertTitle>{t("pages.system.settings.backendDiagnostics.portalError", "Portal Error")}</AlertTitle>
                            <AlertDescription>{backendInfo.portal_error}</AlertDescription>
                        </Alert>
                    )}
                </CardContent>
            </Card>
        </div>
    )
}
