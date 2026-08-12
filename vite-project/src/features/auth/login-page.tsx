import { useState, useEffect, useRef } from "react"
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import * as z from "zod"
import { useNavigate, useSearchParams } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Lock, User } from "lucide-react"

import {
    Form,
    FormControl,
    FormField,
    FormItem,
    FormMessage,
} from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { useToast } from "@/hooks/use-toast"
import { useLoginAccount } from "@/services/hooks/authController/useLoginAccount"
import { useLoginTauri } from "@/services/hooks/authController/useLoginTauri"
import { useRedeemCode } from "@/services/hooks/authController/useRedeemCode"
import { useGetCurrentUser } from "@/services/hooks/userController/useGetCurrentUser"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { startupModeEnum } from "@/services/types"
import { saveSessionGrant, clearSessionGrant } from "@/features/desk/session-grant"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"
import { AsyncButton } from "@/components/async-button"
import { deskErrorMessage, errorCodeOf, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"
import { deskErrorCodeEnum, type LoginOutcomeDto } from "@/services/types"

const LOGIN_ERROR_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.ILLEGAL_CREDENTIALS]: "pages.login.accountLogin.errorMessage",
    [deskErrorCodeEnum.ACCOUNT_LOCKED]: "pages.login.accountLogin.locked",
}

const formSchema = z.object({
    username: z.string().optional(),
    password: z.string().optional(),
    deviceCode: z.string().optional(),
    type: z.string().default("account"),
})

type FormValues = z.infer<typeof formSchema>

export default function LoginPage() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const [searchParams] = useSearchParams()
    const { toast } = useToast()
    const [activeTab, setActiveTab] = useState("account")
    const [tauriAutoLoginPending, setTauriAutoLoginPending] = useState(false)
    const tauriAutoLoginTokenRef = useRef<string | null>(null)
    const [lockedUntilMs, setLockedUntilMs] = useState(0)
    const [nowMs, setNowMs] = useState(() => Date.now())
    const lockedRemainingSec = Math.max(0, Math.ceil((lockedUntilMs - nowMs) / 1000))

    useEffect(() => {
        if (lockedUntilMs <= Date.now()) return
        const timer = window.setInterval(() => {
            const now = Date.now()
            setNowMs(now)
            if (now >= lockedUntilMs) window.clearInterval(timer)
        }, 1000)
        return () => window.clearInterval(timer)
    }, [lockedUntilMs])

    const { mutateAsync: login } = useLoginAccount()
    const { mutateAsync: loginTauri } = useLoginTauri()
    const { mutateAsync: redeem } = useRedeemCode()
    const { refetch: fetchUserInfo } = useGetCurrentUser()
    const { data: serverInfoResp, isLoading: isServerInfoLoading } = useQueryServerInfo()

    const serverInfo = serverInfoResp?.data

    useEffect(() => {
        if (!isServerInfoLoading && serverInfo) {
            if (!serverInfo.initialized) {
                navigate("/init")
            }
        }
    }, [serverInfo, isServerInfoLoading, navigate])

    // Tauri auto-login: detect token in URL params
    useEffect(() => {
        const token = searchParams.get("token")
        if (!token || tauriAutoLoginTokenRef.current === token) return
        tauriAutoLoginTokenRef.current = token
        setTauriAutoLoginPending(true)

        const doTauriLogin = async () => {
            try {
                const response = await loginTauri({ params: { token } })
                toast({
                    title: t("pages.login.success"),
                })
                await fetchUserInfo()

                const startupMode = response.data?.startup_mode
                if (startupMode === startupModeEnum["desk-server"]) {
                    navigate("/system/settings")
                } else {
                    navigate("/desk/list")
                }
                return
            } catch (error) {
                // Token invalid or expired, fall through to normal login form
                console.warn("Tauri auto-login failed:", error)
            }
            // Clean up the token from URL so user sees normal login form
            const newParams = new URLSearchParams(searchParams)
            newParams.delete("token")
            window.history.replaceState({}, "", `${window.location.pathname}${newParams.toString() ? '?' + newParams.toString() : ''}`)
            tauriAutoLoginTokenRef.current = null
            setTauriAutoLoginPending(false)
        }

        doTauriLogin()
    }, [searchParams]) // eslint-disable-line react-hooks/exhaustive-deps

    const form = useForm<FormValues>({
        resolver: zodResolver(formSchema) as any, // Cast to any to avoid strict type mismatch issues
        defaultValues: {
            username: "",
            password: "",
            deviceCode: "",
            type: "account",
        },
    })

    // Redeem an access-grant code into a capability-scoped session, then open the
    // resolved target. This replaces the legacy "device-code login": the redeemer is
    // no longer the owner but a restricted session, so the grant token is stored for
    // this target and carried on every RequestRemoteAccess.
    async function onRedeemCode(values: FormValues) {
        const resp = await redeem({ data: { code: (values.deviceCode || "").trim() } })
        // The kubb client rejects a `success:false` envelope, so reaching here means
        // the redemption succeeded and `data` is present.
        const result = resp?.data
        if (!result?.target_connection_id) {
            throw new Error(t("pages.login.deviceCode.offline"))
        }
        if (result.grant_session_id) {
            // A capability-scoped session: remember the grant + ceiling for this target.
            saveSessionGrant(result.target_connection_id, {
                grantSessionId: result.grant_session_id,
                accessCeiling: result.access_ceiling ?? null,
                source: "device-code",
            })
        } else {
            // Full control (no grant): drop any stale restricted grant for this target
            // so a residual token cannot downgrade the session.
            clearSessionGrant(result.target_connection_id)
        }
        toast({ title: t("pages.login.deviceCode.redeemSuccess") })
        navigate(`/desk/${result.target_connection_id}`)
    }

    async function onSubmit(values: FormValues) {
        if (tauriAutoLoginTokenRef.current !== null) return
        if (values.type !== "device_code" && lockedRemainingSec > 0) return
        try {
            if (values.type === "device_code") {
                await onRedeemCode(values)
                return
            }

            await login({
                data: {
                    username: values.username || "",
                    password: values.password || "",
                }
            })
            toast({
                title: t("pages.login.success"),
            })
            await fetchUserInfo()

            const redirect = searchParams.get("redirect") || "/"
            navigate(redirect)
        } catch (error: unknown) {
            const code = errorCodeOf(error)
            const errorData = (error as { data?: LoginOutcomeDto | null })?.data
            if (code === deskErrorCodeEnum.ACCOUNT_LOCKED) {
                const retryAfterSec = Math.max(1, errorData?.retry_after_sec ?? 1)
                const now = Date.now()
                setNowMs(now)
                setLockedUntilMs(now + retryAfterSec * 1000)
            }
            const errorMsg = deskErrorMessage(
                t,
                LOGIN_ERROR_KEYS,
                code,
                error instanceof Error ? error.message : undefined,
                t("pages.login.failure"),
            )

            toast({
                variant: "destructive",
                title: t("pages.login.failure"),
                description: errorMsg,
            })
        }
    }

    return (
        <div className="flex h-screen w-full items-center justify-center bg-[url('https://mdn.alipayobjects.com/yuyan_qk0oxh/afts/img/V-_oS6r-i7wAAAAAAAAAAAAAFl94AQBr')] bg-cover bg-center">
            <div className="absolute top-4 right-4 flex items-center gap-2">
                <LanguageToggle />
                <ModeToggle />
            </div>
            <Card className="w-[380px] sm:w-[420px] shadow-lg bg-white/90 backdrop-blur-sm dark:bg-slate-950/90">
                <CardHeader className="space-y-1 text-center">
                    <div className="flex justify-center mb-4">
                        <img alt="logo" src="/logo.svg" className="h-10 w-10" />
                    </div>
                    <CardTitle className="text-2xl font-bold">LCXL Web Remote Desk</CardTitle>
                    <CardDescription>
                        {t('pages.layouts.userLayout.title')}
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <Tabs value={activeTab} onValueChange={(val) => {
                        if (tauriAutoLoginPending) return
                        setActiveTab(val);
                        form.setValue("type", val);
                    }} className="w-full">
                        {serverInfo && serverInfo.startup_mode !== startupModeEnum["desk-server"] && (
                            <TabsList className="grid w-full grid-cols-2 mb-4">
                                <TabsTrigger value="account" disabled={tauriAutoLoginPending}>{t('pages.login.accountLogin.tab')}</TabsTrigger>
                                <TabsTrigger value="device_code" disabled={tauriAutoLoginPending}>{t('pages.login.deviceCode.tab')}</TabsTrigger>
                            </TabsList>
                        )}
                        <TabsContent value="account">
                            <Form {...form}>
                                <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
                                    <FormField
                                        control={form.control}
                                        name="username"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <User className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input placeholder={t('pages.login.username.placeholder')} className="pl-9" disabled={tauriAutoLoginPending} {...field} />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <FormField
                                        control={form.control}
                                        name="password"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input type="password" placeholder={t('pages.login.password.placeholder')} className="pl-9" disabled={tauriAutoLoginPending} {...field} />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <AsyncButton
                                        type="submit"
                                        className="w-full"
                                        disabled={lockedRemainingSec > 0}
                                        pending={form.formState.isSubmitting || tauriAutoLoginPending}
                                        pendingLabel={t('pages.login.loggingIn')}
                                    >
                                        {lockedRemainingSec > 0
                                            ? t('pages.login.accountLogin.retryAfter', { seconds: lockedRemainingSec })
                                            : t('pages.login.submit')}
                                    </AsyncButton>
                                </form>
                            </Form>
                        </TabsContent>
                        <TabsContent value="device_code">
                            <Form {...form}>
                                <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
                                    <FormField
                                        control={form.control}
                                        name="deviceCode"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormControl>
                                                    <div className="relative">
                                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground" />
                                                        <Input
                                                            placeholder={t('pages.login.deviceCode.placeholder')}
                                                            className="pl-9"
                                                            maxLength={6}
                                                            disabled={tauriAutoLoginPending}
                                                            {...field}
                                                            onChange={e => {
                                                                e.target.value = e.target.value.toUpperCase();
                                                                field.onChange(e);
                                                            }}
                                                        />
                                                    </div>
                                                </FormControl>
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />
                                    <p className="text-xs text-muted-foreground">{t('pages.login.deviceCode.hint')}</p>
                                    <AsyncButton
                                        type="submit"
                                        className="w-full"
                                        pending={form.formState.isSubmitting || tauriAutoLoginPending}
                                        pendingLabel={t('pages.login.loggingIn')}
                                    >
                                        {t('pages.login.deviceCode.connect')}
                                    </AsyncButton>
                                </form>
                            </Form>
                        </TabsContent>
                    </Tabs>
                </CardContent>
            </Card>
            {/* Footer component placeholder */}
            <div className="fixed bottom-4 w-full text-center text-sm text-gray-500">
                LCXL Remote Desk Design
            </div>
        </div>
    )
}
