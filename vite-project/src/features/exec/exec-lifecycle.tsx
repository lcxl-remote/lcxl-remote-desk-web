import { useTranslation } from "react-i18next"
import { Loader2, Check, Ban, Square } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import type { ExecEntry } from "./use-confirm-exec"

/**
 * The shared sealed-execution lifecycle card for one row that has already been
 * sent for classification: previewing -> a confirmation preview showing the
 * server's classification (impact / policy / timeout) -> explicit Approve or
 * Reject -> result. Nothing runs without an explicit Approve; a non-executable
 * preview (blocked / off-template / mode) is shown but not runnable.
 *
 * Feature-neutral: both the diagnose panel and the terminal copilot render it
 * once `useConfirmExec` has an entry for the row. The initial trigger (the
 * "Execute" / "Run" button shown before any entry exists) stays caller-specific,
 * since each surface places it differently.
 */
export function ExecLifecycle({
    entry,
    onApprove,
    onReject,
    onCancel,
    onDismiss,
}: {
    entry: ExecEntry
    onApprove: () => void
    onReject: () => void
    /** Ask the host to stop a running command. Omitted where no surface offers it. */
    onCancel?: () => void
    onDismiss: () => void
}) {
    const { t } = useTranslation()

    if (entry.phase === "previewing") {
        return (
            <div className="mt-2 flex items-center gap-2 text-xs text-blue-300">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t("pages.exec.classifying")}
            </div>
        )
    }

    if (entry.phase === "awaiting" && entry.preview) {
        const p = entry.preview
        return (
            <div className="mt-2 flex flex-col gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/10 p-2">
                <div className="text-xs font-semibold text-amber-200">
                    {t("pages.exec.confirmTitle")}
                </div>
                <div className="text-xs text-white/80">{p.impact}</div>
                {p.policy_note && (
                    <div className="text-[10px] text-white/50">{p.policy_note}</div>
                )}
                <div className="text-[10px] text-white/50">
                    {t("pages.exec.timeout")}: {Math.round(p.timeout_ms / 1000)}s
                </div>
                <div className="mt-1 flex gap-2">
                    <Button
                        size="sm"
                        className="h-7 flex-1 bg-red-600 text-xs hover:bg-red-700"
                        onClick={onApprove}
                    >
                        <Check className="mr-1 h-3 w-3" />
                        {t("pages.exec.approve")}
                    </Button>
                    <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 flex-1 text-xs"
                        onClick={onReject}
                    >
                        <Ban className="mr-1 h-3 w-3" />
                        {t("pages.exec.reject")}
                    </Button>
                </div>
            </div>
        )
    }

    // Waiting for the host to say it started. Distinct from `running`, which is
    // only ever entered because the host reported it — an approval that never
    // reached a host must not look like a command that is working.
    if (entry.phase === "dispatching") {
        return (
            <div className="mt-2 flex items-center gap-2 text-xs text-blue-300">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t("pages.exec.dispatching")}
            </div>
        )
    }

    if (entry.phase === "running") {
        return (
            <div className="mt-2 flex items-center gap-2 text-xs text-blue-300">
                <Loader2 className="h-3 w-3 animate-spin" />
                <span>
                    {t("pages.exec.running")}
                    {entry.runningMs !== null &&
                        ` (${Math.round(entry.runningMs / 1000)}s)`}
                </span>
                {entry.cancelRequested ? (
                    // The command is not over until the host says so, so the row
                    // keeps showing it as running rather than as stopped.
                    <span className="text-white/50">{t("pages.exec.cancelRequested")}</span>
                ) : (
                    onCancel && (
                        <Button
                            size="sm"
                            variant="ghost"
                            className="h-6 px-2 text-[10px]"
                            onClick={onCancel}
                        >
                            <Square className="mr-1 h-3 w-3" />
                            {t("pages.exec.cancel")}
                        </Button>
                    )
                )}
            </div>
        )
    }

    if (entry.phase === "done" && entry.output) {
        const o = entry.output
        const ok = o.exit_code === 0
        return (
            <div className="mt-2 flex flex-col gap-1 rounded-md border border-white/10 bg-black/40 p-2">
                <div className="flex items-center justify-between">
                    <Badge
                        variant="outline"
                        className={
                            ok
                                ? "bg-green-500/20 text-green-300 border-green-500/40"
                                : "bg-red-500/20 text-red-300 border-red-500/40"
                        }
                    >
                        {t("pages.exec.exit")} {o.exit_code}
                    </Badge>
                    <span className="text-[10px] text-white/40">{o.duration_ms}ms</span>
                </div>
                {o.stdout && (
                    <pre className="max-h-32 overflow-auto whitespace-pre-wrap break-all font-mono text-[10px] text-white/80">
                        {o.stdout}
                        {o.stdout_truncated && " …"}
                    </pre>
                )}
                {o.stderr && (
                    <pre className="max-h-24 overflow-auto whitespace-pre-wrap break-all font-mono text-[10px] text-red-300/80">
                        {o.stderr}
                        {o.stderr_truncated && " …"}
                    </pre>
                )}
                <Button
                    size="sm"
                    variant="ghost"
                    className="h-6 self-end text-[10px]"
                    onClick={onDismiss}
                >
                    {t("pages.exec.dismiss")}
                </Button>
            </div>
        )
    }

    // error (blocked / off-template / mode-disabled / execution failure)
    return (
        <div className="mt-2 flex flex-col gap-1 rounded-md border border-red-500/30 bg-red-500/10 p-2">
            <div className="flex items-start gap-1 text-xs text-red-300">
                <Ban className="mt-0.5 h-3 w-3 shrink-0" />
                <span>{entry.error ?? t("pages.exec.notExecutable")}</span>
            </div>
            <Button
                size="sm"
                variant="ghost"
                className="h-6 self-end text-[10px]"
                onClick={onDismiss}
            >
                {t("pages.exec.dismiss")}
            </Button>
        </div>
    )
}
