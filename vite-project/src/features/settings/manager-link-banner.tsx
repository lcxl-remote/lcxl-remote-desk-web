import { useTranslation } from "react-i18next"
import { AlertTriangle, Loader2, RotateCw } from "lucide-react"

import { useQueryManagerLinkStatus } from "@/services/hooks/managerLinkController/useQueryManagerLinkStatus"
import { useRetryManagerLink } from "@/services/hooks/managerLinkController/useRetryManagerLink"
import { managerLinkReasonKey } from "@/features/settings/manager-link-reason"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import { useToast } from "@/hooks/use-toast"

// Poll the link status while the page is open so the banner clears on its own
// once the user frees a device slot and the proxy re-registers.
const STATUS_POLL_INTERVAL_MS = 10_000

// Shown on the Desk Connection page: when the manager fatally rejects this
// host's registration (device quota full, or a missing device identity), the
// signaling proxy pauses auto-reconnect. This surfaces the reason and offers a
// manual retry the user invokes after freeing a slot from a control end.
export function ManagerLinkBanner() {
    const { t } = useTranslation()
    const { toast } = useToast()

    const { data: statusResponse, refetch } = useQueryManagerLinkStatus({
        query: { refetchInterval: STATUS_POLL_INTERVAL_MS },
    })
    const { mutateAsync: retryLink, isPending: isRetrying } = useRetryManagerLink()

    const status = statusResponse?.data
    if (!status?.blocked) {
        return null
    }

    const onRetry = async () => {
        try {
            await retryLink()
            // Give the proxy a beat to attempt re-registration, then refresh.
            await refetch()
            toast({
                title: t("pages.managerLink.retrySucceed"),
            })
        } catch {
            toast({
                variant: "destructive",
                title: t("pages.managerLink.retryFailed"),
            })
        }
    }

    return (
        <Alert variant="destructive" className="mb-6">
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>{t("pages.managerLink.blockedTitle")}</AlertTitle>
            <AlertDescription className="space-y-3">
                <p>{t(managerLinkReasonKey(status.error_code))}</p>
                {status.message ? (
                    <p className="text-muted-foreground">{status.message}</p>
                ) : null}
                <Button size="sm" variant="outline" onClick={onRetry} disabled={isRetrying}>
                    {isRetrying ? (
                        <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    ) : (
                        <RotateCw className="mr-2 h-4 w-4" />
                    )}
                    {isRetrying ? t("pages.managerLink.retrying") : t("pages.managerLink.retry")}
                </Button>
            </AlertDescription>
        </Alert>
    )
}
