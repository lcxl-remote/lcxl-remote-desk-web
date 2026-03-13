import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2 } from "lucide-react"

import { useQuerySecuritySettings } from "@/services/hooks/undefinedController/useQuerySecuritySettings"
import { useUpdateSecuritySettings } from "@/services/hooks/undefinedController/useUpdateSecuritySettings"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"
import { useQueryClient } from "@tanstack/react-query"
import { querySecuritySettingsQueryKey } from "@/services/hooks/undefinedController/useQuerySecuritySettings"

const securitySettingsSchema = z.object({
    allow_remote_control: z.boolean().nullable(),
    allow_clipboard_sync: z.boolean().nullable(),
    allow_private_screen: z.boolean().nullable(),
    allow_whiteboard: z.boolean().nullable(),
    allow_terminal: z.boolean().nullable(),
    allow_file_browse: z.boolean().nullable(),
    allow_file_transfer: z.boolean().nullable(),
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
            allow_file_transfer: null,
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
                allow_file_transfer: data.allow_file_transfer ?? null,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: SecuritySettingsFormValues) => {
        try {
            await updateSettings({ data: values })
            form.reset(values)
            await queryClient.invalidateQueries({ queryKey: querySecuritySettingsQueryKey() })
            toast({
                title: t('pages.system.security.success', 'Success'),
                description: t('pages.system.security.updateSucceedMessage', "Security settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.security.error', 'Error'),
                description: t('pages.system.security.updateFailedMessage', "Failed to update security settings"),
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
        { name: "allow_remote_control", label: t("security.permission.remoteControl", "Remote Control"), desc: t("pages.system.security.remoteControlDesc", "Allow remote control of desktop") },
        { name: "allow_clipboard_sync", label: t("security.permission.clipboardSync", "Clipboard Sync"), desc: t("pages.system.security.clipboardSyncDesc", "Allow synchronization of clipboard content") },
        { name: "allow_private_screen", label: t("security.permission.privateScreen", "Private Screen"), desc: t("pages.system.security.privateScreenDesc", "Allow enabling privacy screen mode") },
        { name: "allow_whiteboard", label: t("security.permission.whiteboard", "Whiteboard"), desc: t("pages.system.security.whiteboardDesc", "Allow remote drawing on screen") },
        { name: "allow_terminal", label: t("security.permission.terminal", "Terminal Access"), desc: t("pages.system.security.terminalDesc", "Allow access to the remote terminal") },
        { name: "allow_file_browse", label: t("security.permission.fileBrowse", "File Browse"), desc: t("pages.system.security.fileBrowseDesc", "Allow listing and deleting files") },
        { name: "allow_file_transfer", label: t("security.permission.fileTransfer", "File Transfer"), desc: t("pages.system.security.fileTransferDesc", "Allow uploading and downloading files") },
    ]

    return (
        <div className="container mx-auto max-w-4xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.system.security.title', 'Security Settings')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.system.security.description', 'Manage remote connection permissions and approval behaviors')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.system.security.permissions", "Connection Permissions")}</CardTitle>
                    <CardDescription>{t("pages.system.security.permissions.description", "If set to Prompt, users will be asked for permission if a graphical interface is available. Otherwise, requests will be denied.")}</CardDescription>
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
                                                            <SelectItem value="prompt">{t("security.select.prompt", "Ask every time")}</SelectItem>
                                                            <SelectItem value="allow">{t("security.select.allow", "Always allow")}</SelectItem>
                                                            <SelectItem value="deny">{t("security.select.deny", "Always deny")}</SelectItem>
                                                        </SelectContent>
                                                    </Select>
                                                </div>
                                            </FormItem>
                                        )}
                                    />
                                ))}
                            </div>

                            <div className="flex justify-end pt-4">
                                <Button type="submit" disabled={!form.formState.isDirty || isUpdating} className="w-full sm:w-auto">
                                    {isUpdating && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
                                    {t("pages.system.security.save", "Save Changes")}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
