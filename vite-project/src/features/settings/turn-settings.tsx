import { useEffect } from "react"
import { useForm, useFieldArray } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { Loader2, Save, ShieldCheck, RefreshCw, Plus, Trash2 } from "lucide-react"

import { useQueryTurnSettings } from "@/services/hooks/turnController/useQueryTurnSettings"
import { useUpdateTurnSettings } from "@/services/hooks/turnController/useUpdateTurnSettings"
import { useRegenerateTurnSecret } from "@/services/hooks/turnController/useRegenerateTurnSecret"
import type { TurnSettings } from "@/services/types"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import {
    AlertDialog,
    AlertDialogAction,
    AlertDialogCancel,
    AlertDialogContent,
    AlertDialogDescription,
    AlertDialogFooter,
    AlertDialogHeader,
    AlertDialogTitle,
    AlertDialogTrigger,
} from "@/components/ui/alert-dialog"
import { useToast } from "@/hooks/use-toast"

const turnInterfaceSchema = z.object({
    transport: z.enum(["tcp", "udp"]),
    listen: z.string().min(1, "Listen address is required"),
    external: z.string().min(1, "External address is required"),
})

const turnSettingsSchema = z.object({
    realm: z.string().min(1, "Realm is required"),
    interfaces: z.array(turnInterfaceSchema),
    enable_stun: z.boolean(),
    enable_turn: z.boolean(),
    relay_min_port: z.number().min(1).max(65535),
    relay_max_port: z.number().min(1).max(65535),
})

type TurnSettingsFormValues = z.infer<typeof turnSettingsSchema>

export function TurnSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: turnSettingsResponse, isLoading } = useQueryTurnSettings()
    const { mutateAsync: updateTurnSettings, isPending: isUpdating } = useUpdateTurnSettings()
    const { mutateAsync: regenerateSecret, isPending: isRegenerating } = useRegenerateTurnSecret()

    const form = useForm<TurnSettingsFormValues>({
        resolver: zodResolver(turnSettingsSchema),
        defaultValues: {
            realm: "localhost",
            interfaces: [],
            enable_stun: true,
            enable_turn: false,
            relay_min_port: 50000,
            relay_max_port: 50050,
        },
    })

    const { fields, append, remove } = useFieldArray({
        control: form.control,
        name: "interfaces",
    })

    // Update form values once data is loaded
    useEffect(() => {
        if (turnSettingsResponse?.data && !form.formState.isDirty && !isLoading) {
            const data = turnSettingsResponse.data
            form.reset({
                realm: data.realm || "localhost",
                interfaces: data.interfaces || [],
                enable_stun: data.enable_stun ?? true,
                enable_turn: data.enable_turn ?? false,
                relay_min_port: data.relay_min_port || 50000,
                relay_max_port: data.relay_max_port || 50050,
            })
        }
    }, [turnSettingsResponse?.data, isLoading, form])

    const onSubmit = async (values: TurnSettingsFormValues) => {
        try {
            // merge with existing settings to keep static_credentials and static_auth_secret untouched by frontend
            const currentSettings = turnSettingsResponse?.data || {}
            const payload: TurnSettings = {
                ...currentSettings,
                ...values,
            } as TurnSettings;
            await updateTurnSettings({ data: payload })
            toast({
                title: t('pages.system.settings.success', 'Success'),
                description: t('pages.turn.settings.updateSucceedMessage', "TURN settings updated successfully"),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error', 'Error'),
                description: t('pages.turn.settings.updateFailedMessage', "Failed to update TURN settings"),
            })
        }
    }

    const onRegenerateSecret = async () => {
        try {
            await regenerateSecret()
            toast({
                title: t('pages.system.settings.success', 'Success'),
                description: t('pages.system.settings.regenerateSecretSuccess', "TURN secret updated, please restart the server."),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.system.settings.error', 'Error'),
                description: t('pages.system.settings.regenerateSecretError', "Failed to regenerate TURN secret"),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.turn.settings.title', 'TURN Settings')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.turn.settings.description', 'Manage TURN/STUN server configuration')}
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
                    <CardTitle>{t("pages.turn.settings.configuration", "Configuration")}</CardTitle>
                    <CardDescription>{t("pages.turn.settings.configuration.description", "Update the TURN server properties.")}</CardDescription>
                </CardHeader>
                <CardContent>
                    <Form {...form}>
                        <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="realm"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.turn.settings.realm", "Realm")}</FormLabel>
                                            <FormControl>
                                                <Input {...field} />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="grid gap-6 md:grid-cols-2">
                                <FormField
                                    control={form.control}
                                    name="relay_min_port"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.turn.settings.relayMinPort", "Relay Min Port")}</FormLabel>
                                            <FormControl>
                                                <Input type="number" {...field} onChange={e => field.onChange(e.target.value === '' ? 0 : Number(e.target.value))} />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                                <FormField
                                    control={form.control}
                                    name="relay_max_port"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t("pages.turn.settings.relayMaxPort", "Relay Max Port")}</FormLabel>
                                            <FormControl>
                                                <Input type="number" {...field} onChange={e => field.onChange(e.target.value === '' ? 0 : Number(e.target.value))} />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="space-y-4 rounded-md border p-4">
                                <FormField
                                    control={form.control}
                                    name="enable_stun"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.turn.settings.enableStun", "Enable STUN")}</FormLabel>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="enable_turn"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg p-3 shadow-sm">
                                            <div className="space-y-0.5">
                                                <FormLabel>{t("pages.turn.settings.enableTurn", "Enable TURN")}</FormLabel>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
                                        </FormItem>
                                    )}
                                />
                            </div>

                            <div className="space-y-4">
                                <div className="flex items-center justify-between">
                                    <h3 className="text-lg font-medium">{t("pages.turn.settings.interfaces", "Interfaces")}</h3>
                                    <Button
                                        type="button"
                                        variant="outline"
                                        size="sm"
                                        onClick={() => append({ transport: "udp", listen: "0.0.0.0:3478", external: "" })}
                                    >
                                        <Plus className="mr-2 h-4 w-4" />
                                        {t("pages.turn.settings.addInterface", "Add Interface")}
                                    </Button>
                                </div>
                                {fields.length === 0 && (
                                    <p className="text-sm text-muted-foreground italic">
                                        {t("pages.turn.settings.noInterfaces", "No interfaces configured.")}
                                    </p>
                                )}
                                {fields.map((field, index) => (
                                    <div key={field.id} className="grid gap-4 md:grid-cols-4 items-end rounded-md border p-4">
                                        <FormField
                                            control={form.control}
                                            name={`interfaces.${index}.transport`}
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t("pages.turn.settings.transport", "Transport")}</FormLabel>
                                                    <Select onValueChange={field.onChange} value={field.value}>
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder="Select transport" />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            <SelectItem value="udp">UDP</SelectItem>
                                                            <SelectItem value="tcp">TCP</SelectItem>
                                                        </SelectContent>
                                                    </Select>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name={`interfaces.${index}.listen`}
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t("pages.turn.settings.listen", "Listen Address")}</FormLabel>
                                                    <FormControl>
                                                        <Input {...field} placeholder="0.0.0.0:3478" />
                                                    </FormControl>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <FormField
                                            control={form.control}
                                            name={`interfaces.${index}.external`}
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t("pages.turn.settings.external", "External Address")}</FormLabel>
                                                    <FormControl>
                                                        <Input {...field} placeholder="1.2.3.4:3478" />
                                                    </FormControl>
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                        <Button
                                            type="button"
                                            variant="ghost"
                                            size="icon"
                                            className="text-destructive"
                                            onClick={() => remove(index)}
                                        >
                                            <Trash2 className="h-4 w-4" />
                                        </Button>
                                    </div>
                                ))}
                            </div>

                            <div className="flex justify-end">
                                <Button type="submit" disabled={isUpdating}>
                                    {isUpdating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <Save className="mr-2 h-4 w-4" />}
                                    {t('pages.turn.settings.save', 'Save Settings')}
                                </Button>
                            </div>
                        </form>
                    </Form>
                </CardContent>
            </Card>

            <Card className="mt-8 border-destructive/20">
                <CardHeader>
                    <div className="flex items-center gap-2">
                        <ShieldCheck className="h-5 w-5 text-destructive" />
                        <CardTitle>{t("pages.system.settings.turnSecurity", "TURN Security")}</CardTitle>
                    </div>
                    <CardDescription>
                        {t("pages.system.settings.turnSecurity.description", "Manage TURN server security credentials.")}
                    </CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    <p className="text-sm text-muted-foreground">
                        {t("pages.system.settings.regenerateSecretDescription", "Regenerating the secret will invalidate existing TURN sessions and requires a server restart to take effect.")}
                    </p>
                    <AlertDialog>
                        <AlertDialogTrigger asChild>
                            <Button variant="outline" className="text-destructive hover:bg-destructive/10" disabled={isRegenerating}>
                                {isRegenerating ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : <RefreshCw className="mr-2 h-4 w-4" />}
                                {t("pages.system.settings.regenerateSecret", "Regenerate TURN Secret")}
                            </Button>
                        </AlertDialogTrigger>
                        <AlertDialogContent>
                            <AlertDialogHeader>
                                <AlertDialogTitle>{t("pages.system.settings.regenerateSecretConfirm", "Are you sure?")}</AlertDialogTitle>
                                <AlertDialogDescription>
                                    {t("pages.system.settings.regenerateSecretDescription", "This action will update the internal TURN secret. All active TURN connections will eventually fail and a server restart is REQUIRED for the new secret to take effect.")}
                                </AlertDialogDescription>
                            </AlertDialogHeader>
                            <AlertDialogFooter>
                                <AlertDialogCancel>{t("common.cancel", "Cancel")}</AlertDialogCancel>
                                <AlertDialogAction onClick={onRegenerateSecret} className="bg-destructive text-destructive-foreground hover:bg-destructive/90">
                                    {t("common.confirm", "Confirm")}
                                </AlertDialogAction>
                            </AlertDialogFooter>
                        </AlertDialogContent>
                    </AlertDialog>
                </CardContent>
            </Card>
        </div>
    )
}
