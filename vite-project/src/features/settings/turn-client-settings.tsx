import { useEffect, useRef } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save } from "lucide-react"

import { useQueryTurnClientSettings } from "@/services/hooks/undefinedController/useQueryTurnClientSettings"
import { useUpdateTurnClientSettings } from "@/services/hooks/undefinedController/useUpdateTurnClientSettings"
import type { TurnClientSettings } from "@/services/types"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage, FormDescription } from "@/components/ui/form"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { useToast } from "@/hooks/use-toast"

const turnClientSettingsSchema = z.object({
    traversal_mode: z.enum(["turn", "stun", "none"]),
})

type TurnClientSettingsFormValues = z.infer<typeof turnClientSettingsSchema>

export function TurnClientSettingsPage() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: turnClientSettingsResponse, isLoading } = useQueryTurnClientSettings()
    const { mutateAsync: updateTurnClientSettings, isPending: isUpdating } = useUpdateTurnClientSettings()

    const form = useForm<TurnClientSettingsFormValues>({
        resolver: zodResolver(turnClientSettingsSchema),
        defaultValues: {
            traversal_mode: "turn",
        },
    })

    const didHydrateRef = useRef(false)
    useEffect(() => {
        if (turnClientSettingsResponse?.data && !isLoading && !didHydrateRef.current) {
            didHydrateRef.current = true
            const data = turnClientSettingsResponse.data
            form.reset({
                traversal_mode: data.traversal_mode || "turn",
            })
        }
    }, [turnClientSettingsResponse?.data, isLoading, form])

    const onSubmit = async (values: TurnClientSettingsFormValues) => {
        try {
            const payload: TurnClientSettings = {
                ...values,
            } as TurnClientSettings;
            await updateTurnClientSettings({ data: payload })
            toast({
                title: t('pages.system.settings.success', 'Success'),
                description: t('pages.turnClient.settings.updateSucceedMessage', "TURN Client settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error', 'Error'),
                description: t('pages.turnClient.settings.updateFailedMessage', "Failed to update TURN Client settings"),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.turnClient.settings.title', 'TURN Client Settings')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.turnClient.settings.description', 'Manage TURN/STUN traversal mode for this server node.')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.turnClient.settings.configuration", "Relay Configuration")}</CardTitle>
                    <CardDescription>{t("pages.turnClient.settings.configuration.description", "Update the relay strategy to connect through complex NAT environments.")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="traversal_mode"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.turnClient.settings.traversalMode", "Traversal Mode")}</FormLabel>
                                            <Select onValueChange={field.onChange} value={field.value}>
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder="Select traversal mode" />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="turn">TURN (Relay)</SelectItem>
                                                    <SelectItem value="stun">STUN (P2P)</SelectItem>
                                                    <SelectItem value="none">None (Direct)</SelectItem>
                                                </SelectContent>
                                            </Select>
                                            <FormDescription>
                                                {t("pages.turnClient.settings.traversalModeDesc", "Determine how the desktop traffic is relayed to the remote client. TURN provides reliable connection over complex NAT. STUN allows direct P2P. None prevents NAT traversal.")}
                                            </FormDescription>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="flex justify-end">
                                <Button type="submit" disabled={isUpdating}>
                                    {isUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t('common.save', 'Save')}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>
        </div>
    )
}
