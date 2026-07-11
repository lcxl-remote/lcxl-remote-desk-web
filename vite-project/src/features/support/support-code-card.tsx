import { useEffect, useState } from "react"
import { useTranslation } from "react-i18next"
import { Copy, LifeBuoy, Loader2 } from "lucide-react"

import { useSupportStatus } from "@/services/hooks/supportController/useSupportStatus"
import { useStartSupport } from "@/services/hooks/supportController/useStartSupport"
import { useStopSupport } from "@/services/hooks/supportController/useStopSupport"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { useToast } from "@/hooks/use-toast"

// Poll while the page is open so the card reflects the code arriving, the
// countdown, and the session ending (expiry / stop) without a manual refresh.
const STATUS_POLL_INTERVAL_MS = 3_000

/** Format seconds-remaining as `m:ss`, clamped at zero. */
function formatRemaining(seconds: number): string {
    const s = Math.max(0, seconds)
    const m = Math.floor(s / 60)
    const r = s % 60
    return `${m}:${r.toString().padStart(2, "0")}`
}

// Shown on the Desk Connection page: lets the local user open a temporary
// support session on demand. Starting it requests a short-lived code from the
// manager (over the host's regular link), shown here to read out to a supporter
// who redeems it into a capability-scoped grant session. The session ends on
// "end support" (which revokes the code) or the code's expiry.
export function SupportCodeCard() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: statusResponse, refetch } = useSupportStatus({
        query: { refetchInterval: STATUS_POLL_INTERVAL_MS },
    })
    const { mutateAsync: startSupport, isPending: isStarting } = useStartSupport()
    const { mutateAsync: stopSupport, isPending: isStopping } = useStopSupport()

    const status = statusResponse?.data
    const active = status?.active ?? false
    const code = status?.code ?? null
    const expiresAt = status?.expires_at ?? null

    // Live countdown: tick once a second while a code with an expiry is showing.
    const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))
    useEffect(() => {
        if (!active || !expiresAt) return
        const id = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000)
        return () => clearInterval(id)
    }, [active, expiresAt])
    const remaining = expiresAt ? expiresAt - now : 0

    const onStart = async () => {
        try {
            await startSupport()
            await refetch()
        } catch {
            toast({ variant: "destructive", title: t("pages.support.startFailed") })
        }
    }

    const onStop = async () => {
        try {
            await stopSupport()
        } finally {
            await refetch()
        }
    }

    const onCopy = () => {
        if (code) void navigator.clipboard?.writeText(code).catch(() => {})
    }

    return (
        <Card className="mb-6">
            <CardHeader>
                <CardTitle className="flex items-center gap-2">
                    <LifeBuoy className="h-4 w-4" />
                    {t("pages.support.title")}
                </CardTitle>
                <CardDescription>{t("pages.support.description")}</CardDescription>
            </CardHeader>
            <CardContent>
                {!active ? (
                    <Button size="sm" onClick={onStart} disabled={isStarting}>
                        {isStarting ? (
                            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                        ) : (
                            <LifeBuoy className="mr-2 h-4 w-4" />
                        )}
                        {t("pages.support.getCode")}
                    </Button>
                ) : code ? (
                    <div className="space-y-3">
                        <div className="flex items-center gap-3">
                            <span className="font-mono text-2xl font-bold tracking-widest">{code}</span>
                            <Button
                                size="icon"
                                variant="ghost"
                                onClick={onCopy}
                                title={t("pages.support.copy")}
                            >
                                <Copy className="h-4 w-4" />
                            </Button>
                        </div>
                        <p className="text-sm text-muted-foreground">
                            {t("pages.support.expiresIn", { time: formatRemaining(remaining) })}
                        </p>
                        <Button size="sm" variant="outline" onClick={onStop} disabled={isStopping}>
                            {isStopping ? <Loader2 className="mr-2 h-4 w-4 animate-spin" /> : null}
                            {t("pages.support.stop")}
                        </Button>
                    </div>
                ) : (
                    <div className="flex items-center gap-3 text-sm text-muted-foreground">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        <span>{t("pages.support.issuing")}</span>
                        <Button size="sm" variant="ghost" onClick={onStop} disabled={isStopping}>
                            {t("pages.support.cancel")}
                        </Button>
                    </div>
                )}
            </CardContent>
        </Card>
    )
}
