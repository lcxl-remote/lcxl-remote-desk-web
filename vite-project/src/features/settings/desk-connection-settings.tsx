import { useEffect, useState } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useTranslation } from "react-i18next"
import { AlertTriangle, CheckCircle2, Loader2, Save, ShieldCheck, XCircle } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/settingsController/useQuerySettings"
import { useUpdateSettings } from "@/services/hooks/settingsController/useUpdateSettings"
import { useVerifyConnection } from "@/services/hooks/connectionController/useVerifyConnection"
import type { ConnectionVerifyResult } from "@/services/types"
import { mergeSystemSettings } from "@/features/settings/settings-payload"
import { ManagerLinkBanner } from "@/features/settings/manager-link-banner"

import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Form, FormControl, FormDescription, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
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
    // Host-local toggle: disable the manager connection without clearing the
    // address so it can be re-enabled later.
    manager_enabled: z.boolean(),
})

type DeskConnectionFormValues = z.infer<typeof deskConnectionSchema>

// Per-target verification state for the status badges.
type VerifyState =
    | { kind: "idle" }
    | { kind: "unconfigured" }
    | { kind: "disabled" }
    | { kind: "checking" }
    | { kind: "result"; result: ConnectionVerifyResult }

export function DeskConnectionSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading, refetch: refetchSettings } = useQuerySettings()
    const { mutateAsync: updateSettings, isPending: isUpdating } = useUpdateSettings()
    const { mutateAsync: verifyConnection } = useVerifyConnection()

    const [signalingState, setSignalingState] = useState<VerifyState>({ kind: "idle" })
    const [managerState, setManagerState] = useState<VerifyState>({ kind: "idle" })
    const [checkingSignaling, setCheckingSignaling] = useState(false)
    const [checkingManager, setCheckingManager] = useState(false)

    const form = useForm<DeskConnectionFormValues>({
        resolver: zodResolver(deskConnectionSchema),
        defaultValues: {
            signaling_url: null,
            signaling_token: null,
            manager_url: null,
            manager_api_token: null,
            manager_enabled: true,
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
                // Unset / true both mean enabled; only an explicit false disables.
                manager_enabled: data.manager_enabled !== false,
            })
        }
    }, [settingsResponse?.data, isLoading, form])

    // Run one verify probe; returns the result or null on transport failure.
    const runVerify = async (
        target: "signaling" | "manager",
        input: string,
        token: string | null,
    ): Promise<ConnectionVerifyResult | null> => {
        try {
            const res = await verifyConnection({
                data: { target, input, token: token || undefined },
            })
            return res.data ?? null
        } catch {
            return null
        }
    }

    const verifySignaling = async () => {
        const url = form.getValues("signaling_url")
        if (!url) {
            setSignalingState({ kind: "unconfigured" })
            return
        }
        setCheckingSignaling(true)
        setSignalingState({ kind: "checking" })
        const result = await runVerify("signaling", url, form.getValues("signaling_token"))
        setCheckingSignaling(false)
        setSignalingState(
            result
                ? { kind: "result", result }
                : {
                      kind: "result",
                      result: {
                          ok: false,
                          reached: false,
                          auth_ok: false,
                          secure: false,
                          error_code: -1,
                          message: t("pages.deskConnection.verify.transportError"),
                      },
                  },
        )
    }

    const verifyManager = async () => {
        const url = form.getValues("manager_url")
        if (!url) {
            setManagerState({ kind: "unconfigured" })
            return
        }
        if (!form.getValues("manager_enabled")) {
            setManagerState({ kind: "disabled" })
            return
        }
        setCheckingManager(true)
        setManagerState({ kind: "checking" })
        const result = await runVerify("manager", url, form.getValues("manager_api_token"))
        setCheckingManager(false)
        setManagerState(
            result
                ? { kind: "result", result }
                : {
                      kind: "result",
                      result: {
                          ok: false,
                          reached: false,
                          auth_ok: false,
                          secure: false,
                          error_code: -1,
                          message: t("pages.deskConnection.verify.transportError"),
                      },
                  },
        )
    }

    // Auto-run a status check once settings load, for any configured + enabled
    // target. Runs only on the initial load (guarded by the idle state).
    useEffect(() => {
        if (isLoading || !settingsResponse?.data) return
        const data = settingsResponse.data
        if (signalingState.kind === "idle") {
            if (data.signaling_url) {
                void verifySignaling()
            } else {
                setSignalingState({ kind: "unconfigured" })
            }
        }
        if (managerState.kind === "idle") {
            if (!data.manager_url) {
                setManagerState({ kind: "unconfigured" })
            } else if (data.manager_enabled === false) {
                setManagerState({ kind: "disabled" })
            } else {
                void verifyManager()
            }
        }
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [isLoading, settingsResponse?.data])

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

            <Card className="mb-6">
                <CardHeader>
                    <CardTitle>{t("pages.deskConnection.status.title")}</CardTitle>
                    <CardDescription>{t("pages.deskConnection.status.description")}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    <ConnectionStatusRow
                        label={t("pages.deskConnection.status.signaling")}
                        state={signalingState}
                        checking={checkingSignaling}
                        onVerify={verifySignaling}
                    />
                    <ConnectionStatusRow
                        label={t("pages.deskConnection.status.manager")}
                        state={managerState}
                        checking={checkingManager}
                        onVerify={verifyManager}
                    />
                </CardContent>
            </Card>

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
                                    name="manager_enabled"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-center justify-between rounded-lg border p-4">
                                            <div className="space-y-0.5">
                                                <FormLabel className="text-base">{t("pages.deskConnection.managerEnabled")}</FormLabel>
                                                <FormDescription>{t("pages.deskConnection.managerEnabled.description")}</FormDescription>
                                            </div>
                                            <FormControl>
                                                <Switch checked={field.value} onCheckedChange={field.onChange} />
                                            </FormControl>
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

// One status row: a label, a state badge, an optional failure reason, and a
// manual re-verify button.
function ConnectionStatusRow({
    label,
    state,
    checking,
    onVerify,
}: {
    label: string
    state: VerifyState
    checking: boolean
    onVerify: () => void
}) {
    const { t } = useTranslation()

    let badge: React.ReactNode
    let reason: string | null = null
    // A reachable but plaintext (`ws`/`http`) target is working yet unencrypted:
    // surfaced as a warning alongside the OK badge rather than as a failure.
    const insecure = state.kind === "result" && state.result.ok && state.result.secure === false
    switch (state.kind) {
        case "checking":
            badge = (
                <Badge variant="secondary">
                    <Loader2 className="mr-1 h-3 w-3 animate-spin" />
                    {t("pages.deskConnection.status.checking")}
                </Badge>
            )
            break
        case "unconfigured":
            badge = <Badge variant="outline">{t("pages.deskConnection.status.unconfigured")}</Badge>
            break
        case "disabled":
            badge = <Badge variant="outline">{t("pages.deskConnection.status.disabled")}</Badge>
            break
        case "result":
            if (state.result.ok) {
                badge = (
                    <Badge variant="default" className="bg-green-600 hover:bg-green-600">
                        <CheckCircle2 className="mr-1 h-3 w-3" />
                        {t("pages.deskConnection.status.ok")}
                    </Badge>
                )
            } else {
                badge = (
                    <Badge variant="destructive">
                        <XCircle className="mr-1 h-3 w-3" />
                        {t("pages.deskConnection.status.error")}
                    </Badge>
                )
                reason = state.result.message
            }
            break
        default:
            badge = <Badge variant="outline">{t("pages.deskConnection.status.idle")}</Badge>
    }

    return (
        <div className="flex items-center justify-between gap-4">
            <div className="flex items-center gap-3">
                <span className="text-sm font-medium">{label}</span>
                {badge}
                {insecure && (
                    <Badge variant="outline" className="border-amber-500 text-amber-600">
                        <AlertTriangle className="mr-1 h-3 w-3" />
                        {t("pages.deskConnection.status.insecure")}
                    </Badge>
                )}
            </div>
            <div className="flex items-center gap-3">
                {reason && <span className="text-xs text-muted-foreground">{reason}</span>}
                {insecure && (
                    <span className="text-xs text-amber-600">{t("pages.deskConnection.status.insecureHint")}</span>
                )}
                <Button type="button" variant="outline" size="sm" disabled={checking} onClick={onVerify}>
                    {checking ? (
                        <Loader2 className="mr-1 h-3 w-3 animate-spin" />
                    ) : (
                        <ShieldCheck className="mr-1 h-3 w-3" />
                    )}
                    {t("pages.deskConnection.verify.button")}
                </Button>
            </div>
        </div>
    )
}
