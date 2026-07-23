import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2 } from "lucide-react"

import { useQuerySecuritySettings } from "@/services/hooks/securityController/useQuerySecuritySettings"
import { useUpdateSecuritySettings } from "@/services/hooks/securityController/useUpdateSecuritySettings"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel } from "@/components/ui/form"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"
import { useQueryClient } from "@tanstack/react-query"
import { querySecuritySettingsQueryKey } from "@/services/hooks/securityController/useQuerySecuritySettings"
import { mapTimeoutFromSelectValue, mapTimeoutToSelectValue } from "./security-timeout"

const securitySettingsSchema = z.object({
    allow_remote_control: z.boolean().nullable(),
    allow_clipboard_sync: z.boolean().nullable(),
    allow_private_screen: z.boolean().nullable(),
    allow_whiteboard: z.boolean().nullable(),
    allow_terminal: z.boolean().nullable(),
    allow_file_browse: z.boolean().nullable(),
    allow_file_delete: z.boolean().nullable(),
    allow_file_transfer: z.boolean().nullable(),
    approval_timeout: z.number().nullable(),
})

type SecuritySettingsFormValues = z.infer<typeof securitySettingsSchema>

const mapToSelectValue = (val: any) => {
    if (val === true || val === "true") return "allow"
    if (val === false || val === "false") return "deny"
    return "prompt"
}

const mapFromSelectValue = (val: string): boolean | null => {
    if (val === "allow") return true
    if (val === "deny") return false
    return null
}


export function SecuritySettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQuerySecuritySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSecuritySettings()
    const queryClient = useQueryClient()

    const form = useForm<SecuritySettingsFormValues>({
        resolver: zodResolver(securitySettingsSchema),
        defaultValues: {
            allow_remote_control: null,
            allow_clipboard_sync: null,
            allow_private_screen: null,
            allow_whiteboard: null,
            allow_terminal: null,
            allow_file_browse: null,
            allow_file_delete: null,
            allow_file_transfer: null,
            approval_timeout: null,
        },
    })

    // Update form values once data is loaded
    useEffect(() => {
        if (settingsResponse?.data && !form.formState.isDirty && !isLoading) {
            const data = settingsResponse.data
            form.reset({
                allow_remote_control: data.allow_remote_control ?? null,
                allow_clipboard_sync: data.allow_clipboard_sync ?? null,
                allow_private_screen: data.allow_private_screen ?? null,
                allow_whiteboard: data.allow_whiteboard ?? null,
                allow_terminal: data.allow_terminal ?? null,
                allow_file_browse: data.allow_file_browse ?? null,
                allow_file_delete: data.allow_file_delete ?? null,
                allow_file_transfer: data.allow_file_transfer ?? null,
                approval_timeout: data.approval_timeout ?? null,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: SecuritySettingsFormValues) => {
        try {
            await updateSettings({ data: values })
            form.reset(values)
            await queryClient.invalidateQueries({ queryKey: querySecuritySettingsQueryKey() })
            toast({
                title: t('pages.system.security.success'),
                description: t('pages.system.security.updateSucceedMessage'),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.security.error'),
                description: t('pages.system.security.updateFailedMessage'),
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

    const permissionItems = [
        { name: "allow_remote_control", label: t("security.permission.remoteControl"), desc: t("pages.system.security.remoteControlDesc") },
        { name: "allow_clipboard_sync", label: t("security.permission.clipboardSync"), desc: t("pages.system.security.clipboardSyncDesc") },
        { name: "allow_private_screen", label: t("security.permission.privateScreen"), desc: t("pages.system.security.privateScreenDesc") },
        { name: "allow_whiteboard", label: t("security.permission.whiteboard"), desc: t("pages.system.security.whiteboardDesc") },
        { name: "allow_terminal", label: t("security.permission.terminal"), desc: t("pages.system.security.terminalDesc") },
        { name: "allow_file_browse", label: t("security.permission.fileBrowse"), desc: t("pages.system.security.fileBrowseDesc") },
        { name: "allow_file_delete", label: t("security.permission.fileDelete"), desc: t("pages.system.security.fileDeleteDesc") },
        { name: "allow_file_transfer", label: t("security.permission.fileTransfer"), desc: t("pages.system.security.fileTransferDesc") },
    ]

    return (
        <div className="container mx-auto max-w-4xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.system.security.title')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.system.security.description')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.system.security.permissions")}</CardTitle>
                    <CardDescription>{t("pages.system.security.permissions.description")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
                            <div className="space-y-4">
                                {permissionItems.map((item) => (
                                    <FormField
                                        key={item.name}
                                        control={form.control}
                                        name={item.name as keyof SecuritySettingsFormValues}
                                        render={({ field }) => (
                                            <FormItem className="flex flex-row items-center justify-between rounded-lg border p-4">
                                                <div className="space-y-0.5">
                                                    <FormLabel className="text-base">{item.label}</FormLabel>
                                                    <FormDescription>
                                                        {item.desc}
                                                    </FormDescription>
                                                </div>
                                                <div className="w-48 ml-4">
                                                    <Select 
                                                        onValueChange={(val) => {
                                                            if (val && val !== "") {
                                                                field.onChange(mapFromSelectValue(val))
                                                            }
                                                        }} 
                                                        value={mapToSelectValue(field.value)}
                                                    >
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder="Select behavior" />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            <SelectItem value="prompt">{t("security.select.prompt")}</SelectItem>
                                                            <SelectItem value="allow">{t("security.select.allow")}</SelectItem>
                                                            <SelectItem value="deny">{t("security.select.deny")}</SelectItem>
                                                        </SelectContent>
                                                    </Select>
                                                </div>
                                            </FormItem>
                                        )}
                                    />
                                ))}
                            </div>

                            <div className="space-y-4 pt-4 border-t">
                                <h3 className="text-lg font-medium">{t("pages.system.security.behavior")}</h3>
                                <FormField
                                    control={form.control}
                                    name="approval_timeout"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg border p-4">
                                            <div className="space-y-0.5">
                                                <FormLabel className="text-base">{t("security.permission.approvalTimeout")}</FormLabel>
                                                <FormDescription>
                                                    {t("pages.system.security.approvalTimeoutDesc")}
                                                </FormDescription>
                                            </div>
                                            <div className="w-48 ml-4">
                                                <Select
                                                    onValueChange={(val) => {
                                                        if (val !== undefined && val !== null && val !== "") {
                                                            field.onChange(mapTimeoutFromSelectValue(val))
                                                        }
                                                    }}
                                                    value={mapTimeoutToSelectValue(field.value)}
                                                >
                                                    <FormControl>
                                                        <SelectTrigger>
                                                            <SelectValue placeholder="Select timeout" />
                                                        </SelectTrigger>
                                                    </FormControl>
                                                    <SelectContent>
                                                        <SelectItem value="0">{t("security.timeout.never")}</SelectItem>
                                                        <SelectItem value="10">10s</SelectItem>
                                                        <SelectItem value="30">30s</SelectItem>
                                                        <SelectItem value="60">1m</SelectItem>
                                                        <SelectItem value="120">2m</SelectItem>
                                                        <SelectItem value="300">5m</SelectItem>
                                                    </SelectContent>
                                                </Select>
                                            </div>
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="flex justify-end pt-4">
                                <Button type="submit" disabled={!form.formState.isDirty || isUpdating} className="w-full sm:w-auto">
                                    {isUpdating && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                    {t("pages.system.security.save")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
