import { useState } from "react"
import { useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { AlertTriangle, CheckCircle2, ChevronRight, Loader2, Lock, Settings2, User, XCircle } from "lucide-react"
import { useQueryClient } from "@tanstack/react-query"
import { queryServerInfoQueryKey } from "@/services/hooks/systemController/useQueryServerInfo"

import { openExternalUrl } from "@/lib/open-external"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { Checkbox } from "@/components/ui/checkbox"
import { Collapsible, CollapsibleContent } from "@/components/ui/collapsible"
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card"
import { useToast } from "@/hooks/use-toast"
import { useInitSystem } from "@/services/hooks/systemController/useInitSystem"
import { useVerifyConnection } from "@/services/hooks/connectionController/useVerifyConnection"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"
import { TelemetryDisclosure } from "@/components/telemetry-disclosure"
import { AgreementConsent } from "@/components/legal-agreement"
import {
    SECURITY_CAPABILITIES,
    type SecurityToggles,
    buildSecurityPayload,
    isInsecureConnection,
    isManagerConfigured,
    managerNextDecision,
} from "@/features/auth/init/wizard-logic"

// Default manager domain prefilled into the wizard's domain field.
const DEFAULT_MANAGER_DOMAIN = "lcxbox.app"

// Light host[:port] validation: a bare host with an optional port, no scheme or
// path (the backend resolves wss/ws from this).
const DOMAIN_RE = /^[a-zA-Z0-9.-]+(:\d{1,5})?$/

type SchemeStatus =
    | { kind: "idle" }
    | { kind: "checking" }
    | { kind: "ok"; scheme: string; insecure: boolean }
    | { kind: "error"; message: string }

export default function InitPage() {
    const { t } = useTranslation()
    const navigate = useNavigate()
    const { toast } = useToast()
    const queryClient = useQueryClient()

    const { mutateAsync: initSystem } = useInitSystem()
    const { mutateAsync: verifyConnection } = useVerifyConnection()

    const [step, setStep] = useState(1)

    // Step 1: account
    const [username, setUsername] = useState("")
    const [password, setPassword] = useState("")
    const [confirmPassword, setConfirmPassword] = useState("")
    const [agreementAccepted, setAgreementAccepted] = useState(false)

    // Step 2: manager
    const [domain, setDomain] = useState(DEFAULT_MANAGER_DOMAIN)
    const [managerUrl, setManagerUrl] = useState("") // hidden; the persisted value
    const [managerToken, setManagerToken] = useState("")
    const [showAdvanced, setShowAdvanced] = useState(false)
    const [schemeStatus, setSchemeStatus] = useState<SchemeStatus>({ kind: "idle" })
    const [managerNextError, setManagerNextError] = useState<string | null>(null)
    const [resolvingScheme, setResolvingScheme] = useState(false)
    const [verifyingToken, setVerifyingToken] = useState(false)
    const [advancingManager, setAdvancingManager] = useState(false)

    // Step 3: security + telemetry
    const [security, setSecurity] = useState<SecurityToggles>({
        allow_remote_control: false,
        allow_clipboard_sync: false,
        allow_private_screen: false,
        allow_whiteboard: false,
        allow_terminal: false,
        allow_file_browse: false,
        allow_file_transfer: false,
    })
    const [telemetryConsent, setTelemetryConsent] = useState(true)
    const [submitting, setSubmitting] = useState(false)

    const accountValid =
        username.trim().length >= 3 &&
        password.length >= 6 &&
        confirmPassword === password &&
        agreementAccepted

    const managerConfigured = isManagerConfigured(managerUrl, managerToken)

    // Resolve the scheme (wss/ws) for the bare domain on blur, writing the full
    // resolved URL into the hidden manager URL field.
    const resolveDomainScheme = async () => {
        const value = domain.trim()
        if (!value) {
            setSchemeStatus({ kind: "idle" })
            return
        }
        if (!DOMAIN_RE.test(value)) {
            setSchemeStatus({ kind: "error", message: t("pages.init.manager.domainInvalid") })
            return
        }
        setResolvingScheme(true)
        setSchemeStatus({ kind: "checking" })
        try {
            const res = await verifyConnection({ data: { target: "manager", input: value } })
            const result = res.data
            if (result?.reached && result.resolved_url) {
                setManagerUrl(result.resolved_url)
                setSchemeStatus({
                    kind: "ok",
                    scheme: result.scheme || "wss",
                    insecure: isInsecureConnection(result),
                })
            } else {
                setSchemeStatus({
                    kind: "error",
                    message: result?.message || t("pages.init.manager.unreachable"),
                })
            }
        } catch {
            setSchemeStatus({ kind: "error", message: t("pages.init.manager.unreachable") })
        } finally {
            setResolvingScheme(false)
        }
    }

    // The advanced full-URL field was edited: probe it directly (no fallback).
    const resolveAdvancedUrl = async () => {
        const value = managerUrl.trim()
        if (!value) {
            setSchemeStatus({ kind: "idle" })
            return
        }
        setResolvingScheme(true)
        setSchemeStatus({ kind: "checking" })
        try {
            const res = await verifyConnection({ data: { target: "manager", input: value } })
            const result = res.data
            if (result?.reached) {
                setSchemeStatus({
                    kind: "ok",
                    scheme: result.scheme || "",
                    insecure: isInsecureConnection(result),
                })
            } else {
                setSchemeStatus({
                    kind: "error",
                    message: result?.message || t("pages.init.manager.unreachable"),
                })
            }
        } catch {
            setSchemeStatus({ kind: "error", message: t("pages.init.manager.unreachable") })
        } finally {
            setResolvingScheme(false)
        }
    }

    // Optional "verify connection" button: probe with the token for immediate
    // feedback on whether the token is accepted.
    const verifyManagerToken = async () => {
        if (!managerConfigured) return
        setVerifyingToken(true)
        setManagerNextError(null)
        try {
            const res = await verifyConnection({
                data: { target: "manager", input: managerUrl.trim(), token: managerToken.trim() },
            })
            const result = res.data
            const decision = managerNextDecision(result)
            if (decision === "advance") {
                if (isInsecureConnection(result)) {
                    toast({
                        variant: "destructive",
                        title: t("pages.init.manager.verifyOk"),
                        description: t("pages.init.manager.insecureWarning"),
                    })
                } else {
                    toast({ title: t("pages.init.manager.verifyOk") })
                }
            } else if (decision === "token") {
                setManagerNextError(t("pages.init.manager.tokenRejected"))
            } else {
                setManagerNextError(result?.message || t("pages.init.manager.unreachable"))
            }
        } catch {
            setManagerNextError(t("pages.init.manager.unreachable"))
        } finally {
            setVerifyingToken(false)
        }
    }

    const openManagerConsole = () => {
        const host = domain.trim() || DEFAULT_MANAGER_DOMAIN
        // Strip any port for the console origin.
        const hostOnly = host.split(":")[0]
        openExternalUrl(`https://${hostOnly}`)
    }

    // Step 2 "next" for the configure-manager branch: re-verify with the token
    // and advance only if authentication passes.
    const handleManagerNext = async () => {
        if (!managerConfigured) return
        setAdvancingManager(true)
        setManagerNextError(null)
        try {
            const res = await verifyConnection({
                data: { target: "manager", input: managerUrl.trim(), token: managerToken.trim() },
            })
            const result = res.data
            const decision = managerNextDecision(result)
            if (decision === "advance") {
                if (isInsecureConnection(result)) {
                    toast({
                        variant: "destructive",
                        title: t("pages.init.manager.insecureWarningTitle"),
                        description: t("pages.init.manager.insecureWarning"),
                    })
                }
                setStep(3)
            } else if (decision === "token") {
                setManagerNextError(t("pages.init.manager.tokenRejected"))
            } else {
                setManagerNextError(result?.message || t("pages.init.manager.unreachable"))
            }
        } catch {
            setManagerNextError(t("pages.init.manager.unreachable"))
        } finally {
            setAdvancingManager(false)
        }
    }

    // "Skip" is not gated: clear the manager fields and continue.
    const skipManager = () => {
        setDomain("")
        setManagerUrl("")
        setManagerToken("")
        setSchemeStatus({ kind: "idle" })
        setManagerNextError(null)
        setStep(3)
    }

    const finish = async () => {
        setSubmitting(true)
        try {
            const url = managerUrl.trim()
            const token = managerToken.trim()
            await initSystem({
                data: {
                    username: username.trim(),
                    password,
                    telemetry_consent: telemetryConsent,
                    manager_url: url || undefined,
                    manager_api_token: token || undefined,
                    security: buildSecurityPayload(security),
                },
            })
            queryClient.removeQueries({ queryKey: queryServerInfoQueryKey() })
            toast({ title: t("pages.init.success") })
            navigate("/")
        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.init.failure"),
                description: (error as Error).message,
            })
        } finally {
            setSubmitting(false)
        }
    }

    return (
        <div className="flex h-screen w-full items-center justify-center bg-[url('https://mdn.alipayobjects.com/yuyan_qk0oxh/afts/img/V-_oS6r-i7wAAAAAAAAAAAAAFl94AQBr')] bg-cover bg-center">
            <div className="absolute top-4 right-4 flex items-center gap-2">
                <LanguageToggle />
                <ModeToggle />
            </div>

            <div className="flex flex-col items-center animate-in fade-in slide-in-from-bottom-8 duration-1000 px-4">
                <div className="text-center mb-6 space-y-2">
                    <div className="flex justify-center mb-4">
                        <div className="p-3 bg-white/20 backdrop-blur-md rounded-2xl shadow-xl border border-white/30">
                            <img alt="logo" src="/logo.svg" className="h-12 w-12" />
                        </div>
                    </div>
                    <h1 className="text-3xl font-extrabold tracking-tight text-slate-900 dark:text-white drop-shadow-sm sm:text-4xl">
                        {t('pages.init.welcome')}
                    </h1>
                    <p className="text-lg text-slate-600 dark:text-white/80 font-medium">
                        {t('pages.init.subWelcome')}
                    </p>
                </div>

                <Card className="w-full max-w-[480px] shadow-2xl bg-white/95 backdrop-blur-md dark:bg-slate-950/95 border-none">
                    <CardHeader className="space-y-3 pb-2">
                        <StepIndicator step={step} />
                        <div className="text-center">
                            <CardTitle className="text-xl font-bold text-primary">
                                {step === 1 && t('pages.init.step.account')}
                                {step === 2 && t('pages.init.step.manager')}
                                {step === 3 && t('pages.init.step.security')}
                            </CardTitle>
                            <CardDescription className="text-sm">
                                {step === 1 && t('pages.init.step.account.description')}
                                {step === 2 && t('pages.init.step.manager.description')}
                                {step === 3 && t('pages.init.step.security.description')}
                            </CardDescription>
                        </div>
                    </CardHeader>
                    <CardContent className="pt-4">
                        {step === 1 && (
                            <div className="space-y-4">
                                <div className="relative group">
                                    <User className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                    <Input value={username} onChange={e => setUsername(e.target.value)} placeholder={t('pages.init.username.placeholder')} className="pl-9 h-11" />
                                </div>
                                <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                                    <div className="relative group">
                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                        <Input type="password" value={password} onChange={e => setPassword(e.target.value)} placeholder={t('pages.init.password.placeholder')} className="pl-9 h-11" />
                                    </div>
                                    <div className="relative group">
                                        <Lock className="absolute left-3 top-3 h-4 w-4 text-muted-foreground group-focus-within:text-primary transition-colors" />
                                        <Input type="password" value={confirmPassword} onChange={e => setConfirmPassword(e.target.value)} placeholder={t('pages.init.confirmPassword.placeholder')} className="pl-9 h-11" />
                                    </div>
                                </div>
                                {confirmPassword.length > 0 && confirmPassword !== password && (
                                    <p className="text-xs text-destructive">{t('pages.init.confirmPassword.match')}</p>
                                )}
                                <AgreementConsent checked={agreementAccepted} onCheckedChange={setAgreementAccepted} />
                                <Button className="w-full h-11 text-base font-semibold" disabled={!accountValid} onClick={() => setStep(2)}>
                                    {t('pages.init.next')}
                                    <ChevronRight className="ml-1 h-4 w-4" />
                                </Button>
                            </div>
                        )}

                        {step === 2 && (
                            <div className="space-y-4">
                                <div className="space-y-2">
                                    <Label>{t('pages.init.manager.domain')}</Label>
                                    <Input value={domain} onChange={e => setDomain(e.target.value)} onBlur={resolveDomainScheme} placeholder={DEFAULT_MANAGER_DOMAIN} className="h-11" />
                                    <SchemeStatusHint status={schemeStatus} />
                                </div>

                                <Button type="button" variant="ghost" size="sm" className="h-7 px-2 text-xs" onClick={() => setShowAdvanced(v => !v)}>
                                    <Settings2 className="mr-1 h-3 w-3" />
                                    {t('pages.init.manager.advanced')}
                                </Button>
                                <Collapsible open={showAdvanced}>
                                    <CollapsibleContent className="space-y-2">
                                        <Label>{t('pages.init.manager.url')}</Label>
                                        <Input value={managerUrl} onChange={e => setManagerUrl(e.target.value)} onBlur={resolveAdvancedUrl} placeholder="wss://lcxbox.app/api/desk/signaling" className="h-11" />
                                        <p className="text-xs text-muted-foreground">{t('pages.init.manager.url.description')}</p>
                                    </CollapsibleContent>
                                </Collapsible>

                                <div className="space-y-2">
                                    <Label>{t('pages.init.manager.token')}</Label>
                                    <Input value={managerToken} onChange={e => setManagerToken(e.target.value)} placeholder={t('pages.init.manager.token.placeholder')} className="h-11" />
                                </div>

                                <div className="flex flex-wrap gap-2">
                                    <Button type="button" variant="outline" size="sm" onClick={openManagerConsole}>
                                        {t('pages.init.manager.openConsole')}
                                    </Button>
                                    <Button type="button" variant="outline" size="sm" disabled={!managerConfigured || verifyingToken} onClick={verifyManagerToken}>
                                        {verifyingToken && <Loader2 className="mr-1 h-3 w-3 animate-spin" />}
                                        {t('pages.init.manager.verify')}
                                    </Button>
                                </div>

                                {managerNextError && (
                                    <p className="text-xs text-destructive">{managerNextError}</p>
                                )}

                                <div className="flex items-center justify-between gap-2 pt-2">
                                    <Button type="button" variant="ghost" onClick={() => setStep(1)}>
                                        {t('pages.init.back')}
                                    </Button>
                                    <div className="flex gap-2">
                                        <Button type="button" variant="secondary" onClick={skipManager}>
                                            {t('pages.init.skip')}
                                        </Button>
                                        <Button type="button" disabled={!managerConfigured || advancingManager} onClick={handleManagerNext}>
                                            {advancingManager && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
                                            {t('pages.init.next')}
                                            <ChevronRight className="ml-1 h-4 w-4" />
                                        </Button>
                                    </div>
                                </div>
                            </div>
                        )}

                        {step === 3 && (
                            <div className="space-y-4">
                                <div className="space-y-3 rounded-lg border p-4">
                                    {SECURITY_CAPABILITIES.map(cap => (
                                        <div key={cap} className="flex items-center justify-between">
                                            <Label htmlFor={cap} className="text-sm font-normal cursor-pointer">
                                                {t(`pages.init.security.${cap}`)}
                                            </Label>
                                            <Switch id={cap} checked={security[cap]} onCheckedChange={v => setSecurity(s => ({ ...s, [cap]: v }))} />
                                        </div>
                                    ))}
                                    <p className="text-xs text-muted-foreground pt-1">{t('pages.init.security.hint')}</p>
                                </div>

                                <div className="flex flex-row items-start space-x-3 rounded-xl border p-4">
                                    <Checkbox checked={telemetryConsent} onCheckedChange={v => setTelemetryConsent(v === true)} className="mt-1" />
                                    <div className="space-y-1 leading-none">
                                        <Label className="text-sm font-semibold cursor-pointer">{t('pages.init.telemetry.label')}</Label>
                                        <div className="flex items-center gap-2">
                                            <p className="text-xs text-muted-foreground leading-relaxed">{t('pages.init.telemetry.description')}</p>
                                            <TelemetryDisclosure />
                                        </div>
                                    </div>
                                </div>

                                <div className="flex items-center justify-between gap-2 pt-2">
                                    <Button type="button" variant="ghost" onClick={() => setStep(2)}>
                                        {t('pages.init.back')}
                                    </Button>
                                    <Button type="button" className="min-w-32" disabled={submitting} onClick={finish}>
                                        {submitting ? (
                                            <>
                                                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                                                {t('pages.init.submitting')}
                                            </>
                                        ) : (
                                            t('pages.init.finish')
                                        )}
                                    </Button>
                                </div>
                            </div>
                        )}
                    </CardContent>
                </Card>
            </div>

            <div className="fixed bottom-6 w-full text-center text-sm font-medium text-white/60 drop-shadow-md">
                © {new Date().getFullYear()} LCXL Team. Built with Passion.
            </div>
        </div>
    )
}

function StepIndicator({ step }: { step: number }) {
    return (
        <div className="flex items-center justify-center gap-2">
            {[1, 2, 3].map(n => (
                <div key={n} className="flex items-center gap-2">
                    <div
                        className={`flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold transition-colors ${
                            n === step
                                ? "bg-primary text-primary-foreground"
                                : n < step
                                  ? "bg-primary/20 text-primary"
                                  : "bg-muted text-muted-foreground"
                        }`}
                    >
                        {n < step ? <CheckCircle2 className="h-4 w-4" /> : n}
                    </div>
                    {n < 3 && <div className={`h-0.5 w-8 ${n < step ? "bg-primary/40" : "bg-muted"}`} />}
                </div>
            ))}
        </div>
    )
}

function SchemeStatusHint({ status }: { status: SchemeStatus }) {
    const { t } = useTranslation()
    if (status.kind === "idle") return null
    if (status.kind === "checking") {
        return (
            <p className="flex items-center gap-1 text-xs text-muted-foreground">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t('pages.init.manager.resolving')}
            </p>
        )
    }
    if (status.kind === "ok") {
        return (
            <div className="space-y-1">
                <p className="flex items-center gap-1 text-xs text-green-600">
                    <CheckCircle2 className="h-3 w-3" />
                    {status.scheme ? t('pages.init.manager.resolvedScheme', { scheme: status.scheme }) : t('pages.init.manager.reachable')}
                </p>
                {status.insecure && (
                    <p className="flex items-start gap-1 text-xs text-amber-600">
                        <AlertTriangle className="mt-0.5 h-3 w-3 shrink-0" />
                        {t('pages.init.manager.insecureWarning')}
                    </p>
                )}
            </div>
        )
    }
    return (
        <p className="flex items-center gap-1 text-xs text-destructive">
            <XCircle className="h-3 w-3" />
            {status.message}
        </p>
    )
}
