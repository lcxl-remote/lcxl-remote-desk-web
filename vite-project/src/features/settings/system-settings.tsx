import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/undefinedController/useQuerySettings"
import { useUpdateSettings } from "@/services/hooks/undefinedController/useUpdateSettings"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useToast } from "@/hooks/use-toast"

const systemSettingsSchema = z.object({
    open_browser_on_startup: z.boolean(),
    enable_ipv6: z.boolean(),
    traceback: z.boolean(),
    telemetry_consent: z.boolean().nullable(),
    listen_addr_ipv4: z.string().min(1, "IPv4 address is required"),
    listen_addr_ipv6: z.string(),
    signaling_url: z.string().nullable(),
    port: z.number().min(1).max(65535),
    log_level: z.enum(["trace", "debug", "info", "warn", "error"]),
})

type SystemSettingsFormValues = z.infer<typeof systemSettingsSchema>

export function SystemSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQuerySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSettings()

    const form = useForm<SystemSettingsFormValues>({
        resolver: zodResolver(systemSettingsSchema),
        defaultValues: {
            open_browser_on_startup: true,
            enable_ipv6: true,
            traceback: true,
            telemetry_consent: null,
            listen_addr_ipv4: "0.0.0.0",
            listen_addr_ipv6: "::",
            signaling_url: null,
            port: 8081,
            log_level: "info",
        },
    })

    // Update form values once data is loaded
    useEffect(() => {
        if (settingsResponse?.data && !form.formState.isDirty && !isLoading) {
            const data = settingsResponse.data
            form.reset({
                open_browser_on_startup: data.open_browser_on_startup ?? true,
                enable_ipv6: data.enable_ipv6 ?? true,
                traceback: data.traceback ?? true,
                telemetry_consent: data.telemetry_consent ?? null,
                listen_addr_ipv4: data.listen_addr_ipv4 || "0.0.0.0",
                listen_addr_ipv6: data.listen_addr_ipv6 || "::",
                signaling_url: data.signaling_url || null,
                port: data.port || 8081,
                log_level: (data.log_level || "info") as any,
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
                                    name="log_level"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.logLevel", "Log Level")}</FormLabel>
                                            <Select onValueChange={field.onChange} defaultValue={field.value}>
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder="Select log level" />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="trace">TRACE</SelectItem>
                                                    <SelectItem value="debug">DEBUG</SelectItem>
                                                    <SelectItem value="info">INFO</SelectItem>
                                                    <SelectItem value="warn">WARN</SelectItem>
                                                    <SelectItem value="error">ERROR</SelectItem>
                                                </SelectContent>
                                            </Select>
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

                                <FormField
                                    control={form.control}
                                    name="signaling_url"
                                    render={({ field }) => (
                                        <FormItem className="md:col-span-2">
                                            <FormLabel>{t("pages.system.settings.signalingUrl", "Signaling Server URL")}</FormLabel>
                                            <FormControl>
                                                <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="ws://127.0.0.1:8081/signaling" />
                                            </FormControl>
                                            <FormDescription>{t("pages.system.settings.signalingUrl.description", "Leave blank to use the default internal signaling server.")}</FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="space-y-4 rounded-md border p-4">
                                <FormField
                                    control={form.control}
                                    name="open_browser_on_startup"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.openBrowserOnStartup", "Open Browser on Startup")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.openBrowserOnStartup.description", "Automatically launch the web interface when the application starts.")}</FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />

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
                                    name="traceback"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.system.settings.traceback", "Enable Traceback")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.traceback.description", "Record detailed crash stack traces for bug reports.")}</FormDescription>
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
                                                <FormLabel>{t("pages.system.settings.telemetry_consent", "Telemetry Consent")}</FormLabel>
                                                <FormDescription>{t("pages.system.settings.telemetry_consent.tooltip", "Help improve our product by sending anonymous usage data.")}</FormDescription>
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
        </div>
    )
}
