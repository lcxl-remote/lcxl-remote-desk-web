import { useEffect, useRef } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQueryLogSettings } from "@/services/hooks/logController/useQueryLogSettings"
import { useUpdateLogSettings } from "@/services/hooks/logController/useUpdateLogSettings"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"

const logSettingsSchema = z.object({
    log_level: z.enum(["trace", "debug", "info", "warn", "error"]),
    traceback: z.boolean(),
    log_retention_days: z.number().min(1),
    log_cleanup_threshold_percent: z.number().min(1).max(100),
    log_cleanup_interval_hours: z.number().min(1),
})

type LogSettingsFormValues = z.infer<typeof logSettingsSchema>

const LOG_LEVELS = ["trace", "debug", "info", "warn", "error"] as const

export function LogSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQueryLogSettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateLogSettings()

    const form = useForm<LogSettingsFormValues>({
        resolver: zodResolver(logSettingsSchema),
        defaultValues: {
            log_level: "info",
            traceback: true,
            log_retention_days: 7,
            log_cleanup_threshold_percent: 90,
            log_cleanup_interval_hours: 12,
        },
    })

    // Update form values once data is loaded
    const didHydrateRef = useRef(false)
    useEffect(() => {
        if (settingsResponse?.data && !isLoading && !didHydrateRef.current) {
            didHydrateRef.current = true
            const data = settingsResponse.data
            const normalizedLogLevel = typeof data.log_level === "string"
                ? data.log_level.trim().toLowerCase()
                : undefined
            const safeLogLevel = LOG_LEVELS.includes(normalizedLogLevel as (typeof LOG_LEVELS)[number])
                ? (normalizedLogLevel as (typeof LOG_LEVELS)[number])
                : "info"
            form.reset({
                log_level: safeLogLevel,
                traceback: data.traceback ?? true,
                log_retention_days: data.log_retention_days ?? 7,
                log_cleanup_threshold_percent: data.log_cleanup_threshold_percent ?? 90,
                log_cleanup_interval_hours: data.log_cleanup_interval_hours ?? 12,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: LogSettingsFormValues) => {
        try {
            await updateSettings({ data: values })
            toast({
                title: t('pages.system.settings.success', 'Success'),
                description: t('pages.log.settings.updateSucceedMessage', "Log settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error', 'Error'),
                description: t('pages.log.settings.updateFailedMessage', "Failed to update log settings"),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.log.settings.title', 'Log Settings')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.log.settings.description', 'Manage application log levels, formatting, and automatic cleanup rules.')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.log.settings.configuration", "Log Configuration")}</CardTitle>
                    <CardDescription>{t("pages.log.settings.configuration.description", "Update log parameters.")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="log_level"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.logLevel", "Log Level")}</FormLabel>
                                            {/* Radix Select + RHF reset can miss value updates; re-mount to reflect initial data */}
                                            <Select
                                                key={field.value || "log-level-empty"}
                                                onValueChange={field.onChange}
                                                defaultValue={field.value}
                                            >
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
                            </div>

                            <div className="rounded-md border p-4 space-y-4">
                                <h3 className="text-sm font-medium">{t("pages.system.settings.logCleanup.title", "Log Cleanup")}</h3>
                                <div className="grid gap-6 md:grid-cols-3">
                                    <FormField
                                        control={form.control}
                                        name="log_retention_days"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormLabel>{t("pages.system.settings.logCleanup.logRetentionDays", "Retention Days")}</FormLabel>
                                                <FormControl>
                                                    <Input
                                                        key={`log-retention-${field.value ?? "empty"}`}
                                                        type="number"
                                                        {...field}
                                                        value={field.value ?? ""}
                                                        onChange={e => field.onChange(e.target.value === '' ? 7 : Number(e.target.value))}
                                                    />
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <FormField
                                        control={form.control}
                                        name="log_cleanup_threshold_percent"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormLabel>{t("pages.system.settings.logCleanup.logCleanupThresholdPercent", "Disk Threshold (%)")}</FormLabel>
                                                <FormControl>
                                                    <Input
                                                        key={`log-threshold-${field.value ?? "empty"}`}
                                                        type="number"
                                                        {...field}
                                                        value={field.value ?? ""}
                                                        onChange={e => field.onChange(e.target.value === '' ? 90 : Number(e.target.value))}
                                                    />
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <FormField
                                        control={form.control}
                                        name="log_cleanup_interval_hours"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormLabel>{t("pages.system.settings.logCleanup.logCleanupIntervalHours", "Cleanup Interval (Hours)")}</FormLabel>
                                                <FormControl>
                                                    <Input
                                                        key={`log-interval-${field.value ?? "empty"}`}
                                                        type="number"
                                                        {...field}
                                                        value={field.value ?? ""}
                                                        onChange={e => field.onChange(e.target.value === '' ? 12 : Number(e.target.value))}
                                                    />
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                </div>
                            </div>

                            <div className="space-y-4 rounded-md border p-4">
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
