import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/settingsController/useQuerySettings"
import { useUpdateSettings } from "@/services/hooks/settingsController/useUpdateSettings"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { useQueryBackendInfo } from "@/services/hooks/systemController/useQueryBackendInfo"
import { useQueryMacosAutologin } from "@/services/hooks/systemController/useQueryMacosAutologin"
import { mergeSystemSettings } from "@/features/settings/settings-payload"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Badge } from "@/components/ui/badge"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useToast } from "@/hooks/use-toast"
import { TelemetryDisclosure } from "@/components/telemetry-disclosure"
import { ServiceInstallDialog } from "@/features/layout/service-install-dialog"
import { ServiceUninstallDialog } from "@/features/layout/service-uninstall-dialog"
import { useState } from "react"

const systemSettingsSchema = z.object({
    enable_ipv6: z.boolean(),
    telemetry_consent: z.boolean().nullable(),
    auto_start: z.boolean().nullable(),
    host_access_indicator_enabled: z.boolean(),
    listen_addr_ipv4: z.string().min(1, "IPv4 address is required"),
    listen_addr_ipv6: z.string(),
    port: z.number().min(1).max(65535),
})

type SystemSettingsFormValues = z.infer<typeof systemSettingsSchema>

export function SystemSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading, refetch: refetchSettings } = useQuerySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSettings()
    const { data: serverInfoResp } = useQueryServerInfo()
    const { data: backendInfoResp } = useQueryBackendInfo()
    const [installDialogOpen, setInstallDialogOpen] = useState(false)
    const [uninstallDialogOpen, setUninstallDialogOpen] = useState(false)

    const serverInfo = serverInfoResp?.data
    const backendInfo = backendInfoResp?.data

    // macOS-only fields (`background_start` / `macos_permissions`) are `null`
    // on every other platform, so their presence is the platform signal.
    const isMac = serverInfo?.background_start != null

    const form = useForm<SystemSettingsFormValues>({
        resolver: zodResolver(systemSettingsSchema),
        defaultValues: {
            enable_ipv6: true,
            telemetry_consent: null,
            auto_start: null,
            host_access_indicator_enabled: true,
            listen_addr_ipv4: "0.0.0.0",
            listen_addr_ipv6: "::",
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
                host_access_indicator_enabled: data.host_access_indicator_enabled ?? true,
                listen_addr_ipv4: data.listen_addr_ipv4 || "0.0.0.0",
                listen_addr_ipv6: data.listen_addr_ipv6 || "::",
                port: data.port || 8081,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: SystemSettingsFormValues) => {
        try {
            // `update_settings` is a full-struct replace: the payload must carry
            // EVERY SystemSettings field, not just the ones this page renders.
            // Refetch the latest settings and merge this page's edits on top so
            // fields owned by other pages (outbound connection, signal token)
            // and config-only fields (worker_heartbeat_*, webrtc_ice_*) are not
            // wiped. Refetching first also shrinks the lost-update window when a
            // sibling settings page saved concurrently.
            const fresh = await refetchSettings()
            const base = fresh.data?.data ?? settingsResponse?.data ?? {}
            if (
                base.host_access_indicator_enabled !== false
                && !values.host_access_indicator_enabled
                && !window.confirm(t('pages.system.settings.hostAccessIndicator.confirm'))
            ) {
                return
            }
            await updateSettings({ data: mergeSystemSettings(base, values) })
            toast({
                title: t('pages.system.settings.success'),
                description: t('pages.system.settings.updateSucceedMessage'),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error'),
                description: t('pages.system.settings.updateFailedMessage'),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.system.settings.title')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.system.settings.description')}
                </p>
            </div>

            <Alert variant="default" className="mb-6 border-amber-500/50 bg-amber-500/10 text-amber-600 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-500">
                <AlertTitle>{t("pages.system.settings.alert.message")}</AlertTitle>
                <AlertDescription>
                    {t("pages.system.settings.alert.description")}
                </AlertDescription>
            </Alert>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.system.settings.configuration")}</CardTitle>
                    <CardDescription>{t("pages.system.settings.configuration.description")}</CardDescription>
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
                                            <FormLabel>{t("pages.system.settings.port")}</FormLabel>
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
                                            <FormLabel>{t("pages.system.settings.listenAddrIpv4")}</FormLabel>
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
                                            <FormLabel>{t("pages.system.settings.listenAddrIpv6")}</FormLabel>
                                            <FormControl>
                                                <Input {...field} placeholder="::" />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                            </div>

                            <div className="space-y-4 rounded-md border p-4">

                                <FormField
                                    control={form.control}
                                    name="enable_ipv6"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.enableIpv6")}</FormLabel>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="host_access_indicator_enabled"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.hostAccessIndicator")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.hostAccessIndicator.description")}</FormDescription>
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
                                                    {t("pages.system.settings.telemetry_consent")}
                                                    <TelemetryDisclosure />
                                                </FormLabel>
                                                <FormDescription>{t("pages.system.settings.telemetry_consent.tooltip")}</FormDescription>
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
                                                <FormLabel>{t("pages.system.settings.auto_start")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.auto_start.tooltip")}</FormDescription>
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
                                    {t('pages.system.settings.save')}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>

            {!isMac && (serverInfo?.startup_mode === "default" || serverInfo?.startup_mode === "service-daemon") && serverInfo.server_binary_available && (
                <Card className="mt-6 border-amber-500/50 bg-amber-500/10 dark:border-amber-500/30 dark:bg-amber-500/10">
                    <CardHeader>
                        <CardTitle>{t("pages.system.settings.serviceManagement.title")}</CardTitle>
                        <CardDescription>{t("pages.system.settings.serviceManagement.description")}</CardDescription>
                    </CardHeader>
                    <CardContent className="space-y-4">
                        <div className="flex items-center justify-between">
                            <div>
                                <h4 className="text-sm font-medium">{t("pages.system.settings.serviceManagement.status")}</h4>
                                <p className="text-sm text-muted-foreground">
                                    {serverInfo.service_installed 
                                        ? t("pages.system.settings.serviceManagement.installed") 
                                        : t("pages.system.settings.serviceManagement.notInstalled")}
                                </p>
                            </div>
                            {serverInfo.service_installed ? (
                                <Button
                                    variant="destructive"
                                    onClick={() => setUninstallDialogOpen(true)}
                                >
                                    {t("pages.system.settings.serviceManagement.uninstall")}
                                </Button>
                            ) : (
                                <Button onClick={() => setInstallDialogOpen(true)}>
                                    {t("pages.layout.serviceBanner.installButton")}
                                </Button>
                            )}
                        </div>
                    </CardContent>
                </Card>
            )}

            <ServiceInstallDialog
                open={installDialogOpen}
                onOpenChange={setInstallDialogOpen}
                defaultInstallPath={serverInfo?.default_install_path ?? ""}
            />
            <ServiceUninstallDialog
                open={uninstallDialogOpen}
                onOpenChange={setUninstallDialogOpen}
            />

            {isMac && serverInfo && (
                <Card className="mt-6">
                    <CardHeader>
                        <CardTitle>{t("pages.system.settings.macos.title")}</CardTitle>
                        <CardDescription>{t("pages.system.settings.macos.description")}</CardDescription>
                    </CardHeader>
                    <CardContent className="space-y-6">
                        <div className="space-y-2">
                            <h4 className="text-sm font-medium">{t("pages.system.settings.macos.backgroundStart.label")}</h4>
                            <div className="flex items-center gap-2">
                                {!serverInfo.background_start?.configured ? (
                                    <Badge variant="secondary">{t("pages.system.settings.macos.backgroundStart.disabled")}</Badge>
                                ) : serverInfo.background_start.loaded ? (
                                    <Badge variant="default">{t("pages.system.settings.macos.backgroundStart.running")}</Badge>
                                ) : (
                                    <Badge variant="outline">{t("pages.system.settings.macos.backgroundStart.configuredPending")}</Badge>
                                )}
                            </div>
                            {serverInfo.background_start?.configured && !serverInfo.background_start.path_valid && (
                                <Alert variant="destructive">
                                    <AlertTitle>{t("pages.system.settings.macos.backgroundStart.pathInvalidTitle")}</AlertTitle>
                                    <AlertDescription>{t("pages.system.settings.macos.backgroundStart.pathInvalid")}</AlertDescription>
                                </Alert>
                            )}
                        </div>

                        <div className="space-y-3">
                            <h4 className="text-sm font-medium">{t("pages.system.settings.macos.permissions.label")}</h4>
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <p className="text-sm font-medium">{t("pages.system.settings.macos.permissions.screenRecording")}</p>
                                    {!serverInfo.macos_permissions?.screen_recording && (
                                        <p className="text-xs text-muted-foreground">{t("pages.system.settings.macos.permissions.grant")}</p>
                                    )}
                                </div>
                                {serverInfo.macos_permissions?.screen_recording ? (
                                    <Badge variant="default">{t("pages.system.settings.macos.permissions.granted")}</Badge>
                                ) : (
                                    <Badge variant="destructive">{t("pages.system.settings.macos.permissions.notGranted")}</Badge>
                                )}
                            </div>
                            <div className="flex items-center justify-between">
                                <div className="space-y-0.5">
                                    <p className="text-sm font-medium">{t("pages.system.settings.macos.permissions.accessibility")}</p>
                                    {!serverInfo.macos_permissions?.accessibility && (
                                        <p className="text-xs text-muted-foreground">{t("pages.system.settings.macos.permissions.grant")}</p>
                                    )}
                                </div>
                                {serverInfo.macos_permissions?.accessibility ? (
                                    <Badge variant="default">{t("pages.system.settings.macos.permissions.granted")}</Badge>
                                ) : (
                                    <Badge variant="destructive">{t("pages.system.settings.macos.permissions.notGranted")}</Badge>
                                )}
                            </div>
                        </div>
                    </CardContent>
                </Card>
            )}

            <Card className="mt-6">
                <CardHeader>
                    <CardTitle>{t("pages.system.settings.backendDiagnostics")}</CardTitle>
                    <CardDescription>{t("pages.system.settings.backendDiagnostics.description")}</CardDescription>
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
                            <AlertTitle>{t("pages.system.settings.backendDiagnostics.inputError")}</AlertTitle>
                            <AlertDescription>{backendInfo.input_backend_error}</AlertDescription>
                        </Alert>
                    )}
                    {backendInfo?.portal_error && (
                        <Alert variant="destructive" className="mt-2">
                            <AlertTitle>{t("pages.system.settings.backendDiagnostics.portalError")}</AlertTitle>
                            <AlertDescription>{backendInfo.portal_error}</AlertDescription>
                        </Alert>
                    )}
                </CardContent>
            </Card>
        </div>
    )
}
