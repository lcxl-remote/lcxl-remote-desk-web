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

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
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
    const [installDialogOpen, setInstallDialogOpen] = useState(false)
    const [uninstallDialogOpen, setUninstallDialogOpen] = useState(false)

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

                                {serverInfo?.startup_mode !== "signaling" && (
                                    <>
                                        <FormField
                                            control={form.control}
                                            name="signaling_url"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.signalingUrl")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="ws://127.0.0.1:8081/api/desk/signaling" />
                                                    </FormControl>
                                                    <FormDescription>{t("pages.system.settings.signalingUrl.description")}</FormDescription>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="signaling_token"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.signalingToken")}</FormLabel>
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
                                                    <FormLabel>{t("pages.system.settings.managerUrl")}</FormLabel>
                                                    <FormControl>
                                                        <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="ws://manager.example.com/api/desk/signaling" />
                                                    </FormControl>
                                                    <FormDescription>{t("pages.system.settings.managerUrl.description")}</FormDescription>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name="manager_api_token"
                                            render={({ field }) => (
                                                <FormItem className="md:col-span-2">
                                                    <FormLabel>{t("pages.system.settings.managerApiToken")}</FormLabel>
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

            {(serverInfo?.startup_mode === "default" || serverInfo?.startup_mode === "service-daemon") && serverInfo.server_binary_available && (
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
