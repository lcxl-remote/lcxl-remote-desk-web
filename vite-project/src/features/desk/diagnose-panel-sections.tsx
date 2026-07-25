import { useState } from "react"
import { useTranslation } from "react-i18next"
import {
    Ban,
    Check,
    CheckCircle2,
    ChevronRight,
    Clock,
    Loader2,
    Play,
    Terminal as TerminalIcon,
    Wrench,
    XCircle,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { AiGeneratedMark } from "@/components/ai-generated-mark"
import { MarkdownContent } from "@/components/markdown-content"
import { agentErrorMessage } from "@/lib/agent-error-i18n"
import {
    type Confidence,
    type DiagnoseHistoryTurn,
    type RiskLevel,
    type SuggestedCommand,
    type ToolActivity,
    type ToolActivityStatus,
} from "./use-desk-diagnose"
import type { ExecEntry, ExecPreview, ExecRequestInput } from "../exec/use-confirm-exec"
import { ExecLifecycle } from "../exec/exec-lifecycle"

export type ExecControls = {
    entries: Record<number, ExecEntry>
    requestPreview: (rowIndex: number, input: ExecRequestInput) => void
    approve: (rowIndex: number) => void
    reject: (rowIndex: number) => void
    cancel: (rowIndex: number) => void
    dismiss: (rowIndex: number) => void
}

/** Map the backend confidence to a badge colour. */
export function confidenceClass(confidence: Confidence): string {
    switch (confidence) {
        case "high":
            return "bg-green-500/20 text-green-300 border-green-500/40"
        case "medium":
            return "bg-yellow-500/20 text-yellow-300 border-yellow-500/40"
        default:
            return "bg-gray-500/20 text-gray-300 border-gray-500/40"
    }
}

/** Map a suggested-command risk level to a badge colour. */
export function riskClass(risk: RiskLevel): string {
    switch (risk) {
        case "low":
            return "bg-green-500/20 text-green-300 border-green-500/40"
        case "medium":
            return "bg-yellow-500/20 text-yellow-300 border-yellow-500/40"
        case "high":
            return "bg-orange-500/20 text-orange-300 border-orange-500/40"
        default:
            return "bg-red-500/20 text-red-300 border-red-500/40"
    }
}

/**
 * Per-command execution controls (security model §7): Execute -> a confirmation
 * preview showing the server's classification (impact / policy / timeout) ->
 * explicit Approve or Reject -> result. Nothing runs without an explicit
 * Approve; a non-executable preview (blocked / off-template / mode) is shown but
 * not runnable. The lifecycle card is shared with the terminal copilot via
 * `ExecLifecycle`; only the initial Execute trigger is diagnose-specific.
 */
export function ExecRow({
    index,
    command,
    exec,
}: {
    index: number
    command: SuggestedCommand
    exec: ExecControls
}) {
    const { t } = useTranslation()
    const entry = exec.entries[index]

    if (!entry) {
        return (
            <Button
                size="sm"
                variant="secondary"
                className="mt-2 h-7 w-full text-xs"
                onClick={() =>
                    exec.requestPreview(index, {
                        shell: command.shell,
                        command: command.command,
                        // A diagnosis carries no working directory.
                        cwd: null,
                        reason: command.purpose,
                    })
                }
            >
                <Play className="mr-1 h-3 w-3" />
                {t("pages.desk.diagnose.exec.execute")}
            </Button>
        )
    }

    return (
        <ExecLifecycle
            entry={entry}
            onApprove={() => exec.approve(index)}
            onReject={() => exec.reject(index)}
            onCancel={() => exec.cancel(index)}
            onDismiss={() => exec.dismiss(index)}
        />
    )
}

/** Status icon for one tool call in the agentic activity timeline. */
function ToolStatusIcon({ status }: { status: ToolActivityStatus }) {
    switch (status) {
        case "running":
            return <Loader2 className="h-3 w-3 shrink-0 animate-spin text-blue-300" />
        case "awaiting_approval":
            return <Clock className="h-3 w-3 shrink-0 text-amber-300" />
        case "ok":
            return <CheckCircle2 className="h-3 w-3 shrink-0 text-green-300" />
        default:
            return <XCircle className="h-3 w-3 shrink-0 text-red-300" />
    }
}

function formatToolArguments(argumentsJson: string): string {
    if (!argumentsJson.trim()) return "{}"
    try {
        return JSON.stringify(JSON.parse(argumentsJson), null, 2)
    } catch {
        return argumentsJson
    }
}

/**
 * The agentic loop's tool-activity timeline: each tool call the model made this
 * turn, with its live status and expandable model input / redacted output. Empty
 * for the single-turn diagnose path, so it renders nothing there.
 */
export function ToolTimeline({ tools }: { tools: ToolActivity[] }) {
    const { t } = useTranslation()
    if (tools.length === 0) return null
    return (
        <section className="flex flex-col gap-1.5">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                {t("pages.desk.diagnose.toolActivity")}
            </h3>
            <ul className="flex flex-col gap-1">
                {tools.map((tool) => (
                    <li key={tool.callId}>
                        <details className="group rounded border border-white/10 bg-black/10 text-xs text-white/80">
                            <summary className="flex cursor-pointer list-none items-center gap-2 px-2 py-1.5 [&::-webkit-details-marker]:hidden">
                                <ChevronRight className="h-3 w-3 shrink-0 text-white/40 transition-transform group-open:rotate-90" />
                                <ToolStatusIcon status={tool.status} />
                                <Wrench className="h-3 w-3 shrink-0 text-white/40" />
                                <span className="truncate font-mono">{tool.name}</span>
                                {tool.status === "awaiting_approval" && (
                                    <span className="text-amber-300">
                                        {t("pages.desk.diagnose.toolAwaiting")}
                                    </span>
                                )}
                            </summary>
                            <div className="flex flex-col gap-2 border-t border-white/10 px-2 py-2">
                                <div>
                                    <div className="mb-1 font-medium text-white/60">
                                        {t("pages.desk.diagnose.toolInput")}
                                    </div>
                                    <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all rounded bg-black/30 p-2 font-mono text-[11px] text-white/80">
                                        {formatToolArguments(tool.argumentsJson)}
                                    </pre>
                                </div>
                                <div>
                                    <div className="mb-1 font-medium text-white/60">
                                        {t("pages.desk.diagnose.toolOutput")}
                                    </div>
                                    <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all rounded bg-black/30 p-2 font-mono text-[11px] text-white/80">
                                        {tool.output === null
                                            ? t("pages.desk.diagnose.toolOutputPending")
                                            : tool.output ||
                                              t("pages.desk.diagnose.toolOutputEmpty")}
                                    </pre>
                                </div>
                            </div>
                        </details>
                    </li>
                ))}
            </ul>
        </section>
    )
}

/**
 * The agentic loop's mid-run approval card: the model initiated a mutating
 * command and the loop is blocked until the operator approves or rejects it.
 * Unlike `ExecRow` (a suggested command the operator chose to run), this is
 * pushed by the AI itself, so it shows the full command the model wants to run
 * alongside the server's classification (risk / impact / policy / timeout).
 * Nothing runs without an explicit Approve.
 */
export function AgenticExecApproval({
    preview,
    onApprove,
    onReject,
}: {
    preview: ExecPreview
    onApprove: () => void
    onReject: () => void
}) {
    const { t } = useTranslation()
    return (
        <section className="flex flex-col gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/10 p-2">
            <div className="flex items-center justify-between gap-2">
                <span className="text-xs font-semibold text-amber-200">
                    {t("pages.desk.diagnose.exec.agenticTitle")}
                </span>
                <Badge variant="outline" className={riskClass(preview.risk)}>
                    {t(`pages.desk.diagnose.risk.${preview.risk}`, preview.risk)}
                </Badge>
            </div>
            <div className="flex items-center gap-1 text-[10px] uppercase text-white/50">
                <TerminalIcon className="h-3 w-3" />
                {preview.shell}
            </div>
            <pre className="overflow-x-auto whitespace-pre-wrap break-all rounded bg-black/40 p-1.5 font-mono text-xs text-green-300">
                {preview.command}
            </pre>
            <div className="text-xs text-white/80">{preview.impact}</div>
            {preview.execution_basis === "owner_blocklist_only" && (
                <div className="rounded border border-red-500/50 bg-red-950/40 p-1.5 text-[11px] font-medium text-red-200">
                    {t("pages.desk.diagnose.exec.freeformWarning")}
                </div>
            )}
            {preview.policy_note && (
                <div className="text-[10px] text-white/50">{preview.policy_note}</div>
            )}
            <div className="text-[10px] text-white/50">
                {t("pages.desk.diagnose.exec.timeout")}:{" "}
                {Math.round(preview.timeout_ms / 1000)}s
            </div>
            <div className="mt-1 flex gap-2">
                <Button
                    type="button"
                    size="sm"
                    className="h-7 flex-1 bg-red-600 text-xs hover:bg-red-700"
                    onClick={onApprove}
                >
                    <Check className="mr-1 h-3 w-3" />
                    {t("pages.desk.diagnose.exec.approve")}
                </Button>
                <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    className="h-7 flex-1 text-xs"
                    onClick={onReject}
                >
                    <Ban className="mr-1 h-3 w-3" />
                    {t("pages.desk.diagnose.exec.reject")}
                </Button>
            </div>
        </section>
    )
}

/**
 * A right-aligned chat bubble carrying the user's question for a turn. Shared by
 * the settled transcript and the live turn so the current question is visible as
 * soon as it is asked, not only after the next turn snapshots it into history.
 */
export function QuestionBubble({ question }: { question: string }) {
    return (
        <div className="max-w-[85%] self-end rounded-lg rounded-br-sm bg-blue-500/20 px-2.5 py-1.5 text-xs text-white/90">
            {question}
        </div>
    )
}

/**
 * The settled turns of the current conversation, rendered as a compact chat
 * transcript (question bubble + the turn's answer / summary / error) above the
 * live turn. Empty for the first turn, so it renders nothing.
 */
export function ConversationHistory({ turns }: { turns: DiagnoseHistoryTurn[] }) {
    const { t } = useTranslation()
    if (turns.length === 0) return null
    return (
        <div className="flex flex-col gap-3 border-b border-white/10 pb-3">
            {turns.map((turn) => {
                const aiReply =
                    turn.answer ?? turn.result?.summary ?? turn.summary
                const localizedError = turn.error
                    ? agentErrorMessage(t, turn.errorCode, turn.error, turn.error)
                    : null
                return (
                    <div key={turn.requestId} className="flex flex-col gap-1.5">
                        <QuestionBubble question={turn.question} />
                        {/* AI-generated marking (Art.50(2)) for a settled AI
                            answer, mirroring the live turn. Driven by an AI
                            reply being present, not by provenance (fail-closed);
                            an error turn carries no AI content, so no marking. */}
                        {aiReply && (
                            <AiGeneratedMark
                                provenance={turn.provenance}
                                className="self-start border-white/25 bg-white/10 text-white/80"
                            />
                        )}
                        {aiReply && (
                            <MarkdownContent className="max-w-[90%] self-start rounded-lg rounded-bl-sm bg-white/10 px-2.5 py-1.5 text-xs text-white/80">
                                {aiReply}
                            </MarkdownContent>
                        )}
                        <ToolTimeline tools={turn.tools} />
                        {localizedError && (
                            <div className="max-w-[90%] self-start whitespace-pre-wrap rounded-lg rounded-bl-sm bg-red-500/15 px-2.5 py-1.5 text-xs text-red-200">
                                {localizedError}
                            </div>
                        )}
                        {!aiReply && !localizedError && (
                            <div className="max-w-[90%] self-start whitespace-pre-wrap rounded-lg rounded-bl-sm bg-white/10 px-2.5 py-1.5 text-xs text-white/80">
                                {t("pages.desk.diagnose.handedOff")}
                            </div>
                        )}
                    </div>
                )
            })}
        </div>
    )
}

/**
 * A follow-up question composer shown once a turn settles (done or error). It
 * sends another question on the same conversation so the model keeps the prior
 * turns' context.
 */
export function FollowUpComposer({ onSubmit }: { onSubmit: (question: string) => void }) {
    const { t } = useTranslation()
    const [text, setText] = useState("")
    const send = () => {
        const trimmed = text.trim()
        if (!trimmed) return
        onSubmit(trimmed)
        setText("")
    }
    return (
        <div className="flex flex-col gap-2 border-t border-white/10 pt-3">
            <label className="text-xs text-gray-400">
                {t("pages.desk.diagnose.followUpLabel")}
            </label>
            <textarea
                value={text}
                onChange={(e) => setText(e.target.value)}
                rows={2}
                className="w-full resize-none rounded-md border border-white/15 bg-white/5 p-2 text-sm text-white outline-none focus:border-white/40"
                placeholder={t(
                    "pages.desk.diagnose.followUpPlaceholder",
                )}
            />
            <Button size="sm" className="w-full" disabled={!text.trim()} onClick={send}>
                {t("pages.desk.diagnose.followUpSubmit")}
            </Button>
        </div>
    )
}
