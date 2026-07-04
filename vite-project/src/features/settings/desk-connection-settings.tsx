import { useEffect } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/settingsController/useQuerySettings"
import { useUpdateSettings } from "@/services/hooks/settingsController/useUpdateSettings"
import { mergeSystemSettings } from "@/features/settings/settings-payload"
import { ManagerLinkBanner } from "@/features/settings/manager-link-banner"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { useToast } from "@/hooks/use-toast"

// Outbound connection settings let a desk-server reach a standalone signaling
// server or an enterprise manager. They live in the Desk (host) section because
// they configure where THIS desk-server connects out to, not the embedded
// signaling server.
const deskConnectionSchema = z.object({
    signaling_url: z.string().nullable(),
    signaling_token: z.string().nullable(),
    manager_url: z.string().nullable(),
    manager_api_token: z.string().nullable(),
})

type DeskConnectionFormValues = z.infer<typeof deskConnectionSchema>

export function DeskConnectionSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading, refetch: refetchSettings } = useQuerySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSettings()

    const form = useForm<DeskConnectionFormValues>({
        resolver: zodResolver(deskConnectionSchema),
        defaultValues: {
            signaling_url: null,
            signaling_token: null,
            manager_url: null,
            manager_api_token: null,
        },
    })

    useEffect(() => {
        if (settingsResponse?.data && !form.formState.isDirty && !isLoading) {
            const data = settingsResponse.data
            form.reset({
                signaling_url: data.signaling_url || null,
                signaling_token: data.signaling_token || null,
                manager_url: data.manager_url || null,
                manager_api_token: data.manager_api_token || null,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    const onSubmit = async (values: DeskConnectionFormValues) => {
        try {
            // `update_settings` is a full-struct replace, so the payload must
            // carry every SystemSettings field. Refetch the latest settings and
            // merge this page's edits on top so fields owned by other pages
            // (system, signal token) are not wiped, and to shrink the lost-update
            // window when a sibling page saved concurrently.
            const fresh = await refetchSettings()
            const base = fresh.data?.data ?? settingsResponse?.data ?? {}
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.deskConnection.title')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.deskConnection.description')}
                </p>
            </div>

            <ManagerLinkBanner />

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.deskConnection.configuration")}</CardTitle>
                    <CardDescription>{t("pages.deskConnection.configuration.description")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
                            <div className="grid gap-6">
                                <FormField
                                    control={form.control}
                                    name="signaling_url"
                                    render={({ field }) => (
                                        <FormItem>
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
                                        <FormItem>
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
                                        <FormItem>
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
                                        <FormItem>
                                            <FormLabel>{t("pages.system.settings.managerApiToken")}</FormLabel>
                                            <FormControl>
                                                <Input value={field.value ?? ''} onChange={e => field.onChange(e.target.value === '' ? null : e.target.value)} placeholder="Access token for the manager server..." />
                                            </FormControl>
                                            <FormMessage />
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
        </div>
    )
}
