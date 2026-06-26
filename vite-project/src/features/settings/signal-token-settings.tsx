import { useTranslation } from "react-i18next"
import { Loader2, Copy } from "lucide-react"

import { useQuerySettings } from "@/services/hooks/settingsController/useQuerySettings"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useToast } from "@/hooks/use-toast"

// Read-only view of the co-located signaling server's `local_signaling_token`.
// Other desk-servers (and mobile hosts) must present this token to connect to
// this signal, so the operator needs a way to read and copy it. It is never
// edited here — the token is auto-generated and persisted by the server.
export function SignalTokenSettings() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: settingsResponse, isLoading } = useQuerySettings()
    const token = settingsResponse?.data?.local_signaling_token ?? null

    const onCopy = async () => {
        if (!token) return
        try {
            await navigator.clipboard.writeText(token)
            toast({
                title: t('pages.signalToken.copySuccess'),
            })
        } catch (error) {
            toast({
                variant: "destructive",
                title: t('pages.signalToken.copyFailed'),
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
                <h1 className="text-3xl font-bold tracking-tight">{t('pages.signalToken.title')}</h1>
                <p className="text-muted-foreground">
                    {t('pages.signalToken.description')}
                </p>
            </div>

            <Card>
                <CardHeader>
                    <CardTitle>{t("pages.signalToken.configuration")}</CardTitle>
                    <CardDescription>{t("pages.signalToken.configuration.description")}</CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    {token ? (
                        <div className="flex items-center gap-2">
                            <Input readOnly value={token} className="font-mono" />
                            <Button type="button" variant="outline" onClick={onCopy}>
                                <Copy className="mr-2 h-4 w-4" />
                                {t('pages.signalToken.copy')}
                            </Button>
                        </div>
                    ) : (
                        <Alert>
                            <AlertTitle>{t('pages.signalToken.unavailable.title')}</AlertTitle>
                            <AlertDescription>{t('pages.signalToken.unavailable.description')}</AlertDescription>
                        </Alert>
                    )}
                    <Alert variant="default" className="border-amber-500/50 bg-amber-500/10 text-amber-600 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-500">
                        <AlertTitle>{t('pages.signalToken.security.title')}</AlertTitle>
                        <AlertDescription>{t('pages.signalToken.security.description')}</AlertDescription>
                    </Alert>
                </CardContent>
            </Card>
        </div>
    )
}
