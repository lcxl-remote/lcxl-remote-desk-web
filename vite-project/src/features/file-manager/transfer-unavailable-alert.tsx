/**
 * Why file transfers are unavailable, and the evidence behind it.
 *
 * Browsing keeps working when the data channel does not, so this is a banner
 * rather than a page-level failure: it names the stage that failed, offers a
 * retry, and — folded away — shows what was actually observed. That last part
 * matters because the fact which usually decides the case, a TURN server
 * configured but zero relay candidates gathered, is otherwise visible only in
 * the browser's own WebRTC internals.
 */
import { useState } from "react"
import { useTranslation } from "react-i18next"
import { AlertTriangle, Check, Copy, RefreshCw } from "lucide-react"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Button } from "@/components/ui/button"
import {
    ICE_CANDIDATE_TYPES,
    diagnosisKey,
    formatDiagnostics,
} from "./connection-diagnostics"
import type { ConnectionFailureKind, TransferChannelFailure } from "./use-file-transfer"

/**
 * Locally-detected failures, by the stage they happened in.
 *
 * Host refusals are not in here: those carry a `DeskErrorCode` and the host's
 * own text, and follow the file manager's usual policy of showing that text.
 */
export const CONNECTION_FAILURE_KEYS: Record<ConnectionFailureKind, string> = {
    "session-timeout": "pages.fileManager.connection.sessionTimeout",
    "session-closed": "pages.fileManager.connection.sessionClosed",
    "channel-timeout": "pages.fileManager.connection.channelTimeout",
    "ice-failed": "pages.fileManager.connection.iceFailed",
    "channel-closed": "pages.fileManager.connection.channelClosed",
}

export function TransferUnavailableAlert({
    failure,
    onRetry,
}: {
    failure: TransferChannelFailure
    /** Start another attempt. The banner is only shown for a failed channel, so
     * a retry replaces it with the connecting state rather than a spinner here. */
    onRetry: () => void
}) {
    const { t } = useTranslation()
    const [copied, setCopied] = useState(false)
    const { diagnostics } = failure

    const reason = failure.kind
        ? t(CONNECTION_FAILURE_KEYS[failure.kind])
        : failure.message && failure.message.length > 0
            ? failure.message
            : t("common.unknownError")

    const candidateSummary = ICE_CANDIDATE_TYPES
        .map((type) => `${type}=${diagnostics.candidateCounts[type]}`)
        .join("  ")

    const handleCopy = async () => {
        // No clipboard API, no claim that anything was copied: the details are on
        // screen either way and can be selected by hand.
        if (!navigator.clipboard?.writeText) return
        try {
            await navigator.clipboard.writeText(formatDiagnostics(diagnostics))
            setCopied(true)
            setTimeout(() => setCopied(false), 2000)
        } catch {
            // Clipboard access can be denied; same story.
        }
    }

    return (
        <Alert className="mx-4" variant="destructive">
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>{t("pages.fileManager.transferUnavailable.title")}</AlertTitle>
            <AlertDescription>
                <p>{reason}</p>
                <p className="mt-1">{t("pages.fileManager.transferUnavailable.browsingStillWorks")}</p>
                <p className="mt-1 font-medium">{t(diagnosisKey(diagnostics))}</p>

                <details className="mt-2">
                    <summary className="cursor-pointer text-sm">
                        {t("pages.fileManager.diagnostics.title")}
                    </summary>
                    <dl className="mt-2 space-y-1 text-xs font-mono">
                        <div>
                            <dt className="inline font-sans">
                                {t("pages.fileManager.diagnostics.iceServers")}:{" "}
                            </dt>
                            <dd className="inline break-all">
                                {diagnostics.iceServerUrls.length > 0
                                    ? diagnostics.iceServerUrls.join(", ")
                                    : t("pages.fileManager.diagnostics.none")}
                            </dd>
                        </div>
                        <div>
                            <dt className="inline font-sans">
                                {t("pages.fileManager.diagnostics.candidates")}:{" "}
                            </dt>
                            <dd className="inline">{candidateSummary}</dd>
                        </div>
                        <div>
                            <dt className="inline font-sans">
                                {t("pages.fileManager.diagnostics.states")}:{" "}
                            </dt>
                            <dd className="inline">
                                gathering={diagnostics.gatheringState ?? "?"}{" "}
                                ice={diagnostics.iceConnectionState ?? "?"}
                            </dd>
                        </div>
                    </dl>
                </details>

                <div className="mt-3 flex items-center gap-2">
                    <Button size="sm" variant="outline" onClick={onRetry}>
                        <RefreshCw className="h-3.5 w-3.5 mr-1" />
                        {t("pages.fileManager.transferUnavailable.retry")}
                    </Button>
                    <Button size="sm" variant="ghost" onClick={() => void handleCopy()}>
                        {copied ? (
                            <Check className="h-3.5 w-3.5 mr-1" />
                        ) : (
                            <Copy className="h-3.5 w-3.5 mr-1" />
                        )}
                        {copied
                            ? t("pages.fileManager.diagnostics.copied")
                            : t("pages.fileManager.diagnostics.copy")}
                    </Button>
                </div>
            </AlertDescription>
        </Alert>
    )
}
