import { useState } from "react"
import { useTranslation } from "react-i18next"
import { Loader2, Stethoscope, X, UserCog, AlertCircle, Play, Check, Ban, Terminal as TerminalIcon, Wrench, Clock, CheckCircle2, XCircle } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import {
    extractStreamingSummary,
    type Confidence,
    type DiagnoseHistoryTurn,
    type DiagnoseState,
    type DiagnoseStartOptions,
    type RiskLevel,
    type SuggestedCommand,
    type ToolActivity,
    type ToolActivityStatus,
} from "./use-desk-diagnose"
import type { ExecEntry, ExecPreview, ExecRequestInput } from "../exec/use-confirm-exec"

type ExecControls = {
    entries: Record<number, ExecEntry>
    requestPreview: (rowIndex: number, input: ExecRequestInput) => void
    approve: (rowIndex: number) => void
    reject: (rowIndex: number) => void
    dismiss: (rowIndex: number) => void
}

type DiagnosePanelProps = {
    state: DiagnoseState
    onStart: (question: string, options: DiagnoseStartOptions) => void
    onHandoff: () => void
    onReset: () => void
    onClose: () => void
    /** Live signaling connection state; a drop while running is surfaced so the
     * user can start over instead of staring at a stuck spinner. */
    isConnected?: boolean
    /** Confirmed-execution controls; omitted in suggest-only contexts. */
    exec?: ExecControls
    /** Approve the command the agentic loop is parked on (agentic path). */
    onApproveExec?: () => void
    /** Reject the command the agentic loop is parked on (agentic path). */
    onRejectExec?: () => void
}

/** Map the backend confidence to a badge colour. */
function confidenceClass(confidence: Confidence): string {
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
function riskClass(risk: RiskLevel): string {
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
 * not runnable.
 */
function ExecRow({
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

    if (entry.phase === "previewing") {
        return (
            <div className="mt-2 flex items-center gap-2 text-xs text-blue-300">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t("pages.desk.diagnose.exec.classifying")}
            </div>
        )
    }

    if (entry.phase === "awaiting" && entry.preview) {
        const p = entry.preview
        return (
            <div className="mt-2 flex flex-col gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/10 p-2">
                <div className="text-xs font-semibold text-amber-200">
                    {t("pages.desk.diagnose.exec.confirmTitle")}
                </div>
                <div className="text-xs text-white/80">{p.impact}</div>
                {p.policy_note && (
                    <div className="text-[10px] text-white/50">{p.policy_note}</div>
                )}
                <div className="text-[10px] text-white/50">
                    {t("pages.desk.diagnose.exec.timeout")}: {Math.round(p.timeout_ms / 1000)}s
                </div>
                <div className="mt-1 flex gap-2">
                    <Button
                        size="sm"
                        className="h-7 flex-1 bg-red-600 text-xs hover:bg-red-700"
                        onClick={() => exec.approve(index)}
                    >
                        <Check className="mr-1 h-3 w-3" />
                        {t("pages.desk.diagnose.exec.approve")}
                    </Button>
                    <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 flex-1 text-xs"
                        onClick={() => exec.reject(index)}
                    >
                        <Ban className="mr-1 h-3 w-3" />
                        {t("pages.desk.diagnose.exec.reject")}
                    </Button>
                </div>
            </div>
        )
    }

    if (entry.phase === "running") {
        return (
            <div className="mt-2 flex items-center gap-2 text-xs text-blue-300">
                <Loader2 className="h-3 w-3 animate-spin" />
                {t("pages.desk.diagnose.exec.running")}
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
                        {t("pages.desk.diagnose.exec.exit")} {o.exit_code}
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
                    onClick={() => exec.dismiss(index)}
                >
                    {t("pages.desk.diagnose.exec.dismiss")}
                </Button>
            </div>
        )
    }

    // error (blocked / off-template / execution failure)
    return (
        <div className="mt-2 flex flex-col gap-1 rounded-md border border-red-500/30 bg-red-500/10 p-2">
            <div className="flex items-start gap-1 text-xs text-red-300">
                <Ban className="mt-0.5 h-3 w-3 shrink-0" />
                <span>{entry.error ?? t("pages.desk.diagnose.exec.notExecutable")}</span>
            </div>
            <Button
                size="sm"
                variant="ghost"
                className="h-6 self-end text-[10px]"
                onClick={() => exec.dismiss(index)}
            >
                {t("pages.desk.diagnose.exec.dismiss")}
            </Button>
        </div>
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

/**
 * The agentic loop's tool-activity timeline: each tool call the model made this
 * turn, with its live status (running / awaiting approval / ok / failed). Empty
 * for the single-turn diagnose path, so it renders nothing there.
 */
function ToolTimeline({ tools }: { tools: ToolActivity[] }) {
    const { t } = useTranslation()
    if (tools.length === 0) return null
    return (
        <section className="flex flex-col gap-1.5">
            <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                {t("pages.desk.diagnose.toolActivity")}
            </h3>
            <ul className="flex flex-col gap-1">
                {tools.map((tool) => (
                    <li
                        key={tool.callId}
                        className="flex items-center gap-2 text-xs text-white/80"
                    >
                        <ToolStatusIcon status={tool.status} />
                        <Wrench className="h-3 w-3 shrink-0 text-white/40" />
                        <span className="truncate font-mono">{tool.name}</span>
                        {tool.status === "awaiting_approval" && (
                            <span className="text-amber-300">
                                {t("pages.desk.diagnose.toolAwaiting")}
                            </span>
                        )}
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
function AgenticExecApproval({
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
            {preview.policy_note && (
                <div className="text-[10px] text-white/50">{preview.policy_note}</div>
            )}
            <div className="text-[10px] text-white/50">
                {t("pages.desk.diagnose.exec.timeout")}:{" "}
                {Math.round(preview.timeout_ms / 1000)}s
            </div>
            <div className="mt-1 flex gap-2">
                <Button
                    size="sm"
                    className="h-7 flex-1 bg-red-600 text-xs hover:bg-red-700"
                    onClick={onApprove}
                >
                    <Check className="mr-1 h-3 w-3" />
                    {t("pages.desk.diagnose.exec.approve")}
                </Button>
                <Button
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
 * The settled turns of the current conversation, rendered as a compact chat
 * transcript (question bubble + the turn's answer / summary / error) above the
 * live turn. Empty for the first turn, so it renders nothing.
 */
function ConversationHistory({ turns }: { turns: DiagnoseHistoryTurn[] }) {
    const { t } = useTranslation()
    if (turns.length === 0) return null
    return (
        <div className="flex flex-col gap-3 border-b border-white/10 pb-3">
            {turns.map((turn) => {
                const reply = turn.error
                    ? turn.error
                    : turn.answer ?? turn.result?.summary ?? turn.summary
                return (
                    <div key={turn.requestId} className="flex flex-col gap-1.5">
                        <div className="max-w-[85%] self-end rounded-lg rounded-br-sm bg-blue-500/20 px-2.5 py-1.5 text-xs text-white/90">
                            {turn.question}
                        </div>
                        <div
                            className={`max-w-[90%] self-start whitespace-pre-wrap rounded-lg rounded-bl-sm px-2.5 py-1.5 text-xs ${
                                turn.phase === "error"
                                    ? "bg-red-500/15 text-red-200"
                                    : "bg-white/10 text-white/80"
                            }`}
                        >
                            {reply ||
                                t(
                                    "pages.desk.diagnose.handedOff",
                                )}
                        </div>
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
function FollowUpComposer({ onSubmit }: { onSubmit: (question: string) => void }) {
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

export function DiagnosePanel({
    state,
    onStart,
    onHandoff,
    onReset,
    onClose,
    isConnected = true,
    exec,
    onApproveExec,
    onRejectExec,
}: DiagnosePanelProps) {
    const { t, i18n } = useTranslation()
    const [question, setQuestion] = useState("")
    const [includeScreen, setIncludeScreen] = useState(false)

    const presets: string[] = [
        t("pages.desk.diagnose.presetCpu"),
        t("pages.desk.diagnose.presetPort"),
        t("pages.desk.diagnose.presetContainer"),
    ]

    const submit = (q: string) => {
        const trimmed = q.trim()
        if (!trimmed) return
        // Pass the current UI language so the AI answers in it.
        onStart(trimmed, { includeScreen, locale: i18n.language })
    }

    // A follow-up keeps the same conversation; the hook reuses the conversation
    // id so the model sees the prior turns.
    const askFollowUp = (q: string) => onStart(q, { includeScreen, locale: i18n.language })

    // The lifecycle status name is a backend-provided phase string; map the
    // known ones to localized labels, falling back to the raw value.
    const statusLabel = (phase: string | null): string => {
        switch (phase) {
            case "collecting":
                return t("pages.desk.diagnose.statusCollecting")
            case "redacting":
                return t("pages.desk.diagnose.statusRedacting")
            case "modeling":
                return t("pages.desk.diagnose.statusModeling")
            default:
                return phase ?? t("pages.desk.diagnose.statusRunning")
        }
    }

    const result = state.result
    // Show flowing summary text while streaming instead of the raw JSON the
    // model emits under a constrained `response_format`.
    const streamingSummary = extractStreamingSummary(state.partialSummary)

    return (
        <div className="absolute top-4 right-4 z-50 flex w-[380px] max-w-[90vw] max-h-[calc(100%-2rem)] flex-col rounded-lg border border-white/20 bg-black/70 text-white shadow-xl backdrop-blur-md select-text">
            {/* Header */}
            <div className="flex items-center justify-between border-b border-white/15 px-4 py-3">
                <div className="flex items-center gap-2 text-sm font-bold text-white/90">
                    <Stethoscope className="h-4 w-4" style={{ stroke: "url(#ai-rainbow-gradient)" }} />
                    {t("pages.desk.diagnose.title")}
                </div>
                <button
                    onClick={onClose}
                    className="text-gray-400 transition-colors hover:text-white"
                    aria-label={t("pages.desk.diagnose.close")}
                >
                    <X className="h-4 w-4" />
                </button>
            </div>

            <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3 text-sm">
                {/* Conversation transcript (prior settled turns) */}
                {(state.phase === "running" ||
                    state.phase === "done" ||
                    state.phase === "error") && (
                    <div className="mb-3">
                        <ConversationHistory turns={state.history} />
                    </div>
                )}

                {/* Question form (idle) */}
                {state.phase === "idle" && (
                    <div className="flex flex-col gap-3">
                        <label className="text-xs text-gray-400">
                            {t("pages.desk.diagnose.questionLabel")}
                        </label>
                        <textarea
                            value={question}
                            onChange={(e) => setQuestion(e.target.value)}
                            rows={3}
                            className="w-full resize-none rounded-md border border-white/15 bg-white/5 p-2 text-sm text-white outline-none focus:border-white/40"
                            placeholder={t(
                                "pages.desk.diagnose.questionPlaceholder",
                            )}
                        />

                        <div className="flex flex-col gap-1">
                            <span className="text-xs text-gray-400">
                                {t("pages.desk.diagnose.presets")}
                            </span>
                            <div className="flex flex-col gap-1">
                                {presets.map((p) => (
                                    <button
                                        key={p}
                                        onClick={() => setQuestion(p)}
                                        className="rounded-md border border-white/10 bg-white/5 px-2 py-1 text-left text-xs text-white/80 transition-colors hover:bg-white/10"
                                    >
                                        {p}
                                    </button>
                                ))}
                            </div>
                        </div>

                        <label className="flex items-center gap-2 text-xs text-gray-300">
                            <input
                                type="checkbox"
                                checked={includeScreen}
                                onChange={(e) => setIncludeScreen(e.target.checked)}
                                className="h-3.5 w-3.5"
                            />
                            {t("pages.desk.diagnose.includeScreen")}
                        </label>

                        <Button
                            size="sm"
                            className="w-full"
                            disabled={!question.trim()}
                            onClick={() => submit(question)}
                        >
                            {t("pages.desk.diagnose.submit")}
                        </Button>
                    </div>
                )}

                {/* Running: status + streaming summary */}
                {state.phase === "running" && (
                    <div className="flex flex-col gap-3">
                        <div className="flex items-center gap-2 text-xs text-blue-300">
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                            {statusLabel(state.status)}
                        </div>
                        {streamingSummary && (
                            <p className="whitespace-pre-wrap text-sm text-white/90">
                                {streamingSummary}
                            </p>
                        )}
                        <ToolTimeline tools={state.tools} />
                        {state.pendingExec && onApproveExec && onRejectExec && (
                            <AgenticExecApproval
                                preview={state.pendingExec}
                                onApprove={onApproveExec}
                                onReject={onRejectExec}
                            />
                        )}
                        {!isConnected && (
                            <div className="flex items-start gap-2 text-xs text-amber-300">
                                <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                                {t(
                                    "pages.desk.diagnose.connectionLost",
                                )}
                            </div>
                        )}
                    </div>
                )}

                {/* Error */}
                {state.phase === "error" && (
                    <div className="flex flex-col gap-3">
                        <div className="flex items-start gap-2 text-sm text-red-300">
                            <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                            <span>{state.error}</span>
                        </div>
                        {/* A failed turn is settled, so a follow-up may continue the
                            same conversation (the backend allows re-claiming it). */}
                        <FollowUpComposer onSubmit={askFollowUp} />
                    </div>
                )}

                {/* Result (done) */}
                {state.phase === "done" && (
                    <div className="flex flex-col gap-4">
                        {result ? (
                            <>
                                {/* Summary + confidence */}
                                <section className="flex flex-col gap-2">
                                    <div className="flex items-center justify-between">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t("pages.desk.diagnose.summary")}
                                        </h3>
                                        <Badge
                                            variant="outline"
                                            className={confidenceClass(result.confidence)}
                                        >
                                            {t(
                                                `pages.desk.diagnose.confidence.${result.confidence}`,
                                                result.confidence,
                                            )}
                                        </Badge>
                                    </div>
                                    <p className="whitespace-pre-wrap text-sm text-white/90">
                                        {result.summary}
                                    </p>
                                </section>

                                {/* Findings / evidence */}
                                {result.findings.length > 0 && (
                                    <section className="flex flex-col gap-2">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t("pages.desk.diagnose.findings")}
                                        </h3>
                                        {result.findings.map((f, i) => (
                                            <div
                                                key={i}
                                                className="rounded-md border border-white/10 bg-white/5 p-2"
                                            >
                                                <div className="text-sm font-medium text-white/90">
                                                    {f.title}
                                                </div>
                                                <p className="mt-1 text-xs text-white/70">
                                                    {f.explanation}
                                                </p>
                                                {f.evidence_refs.length > 0 && (
                                                    <div className="mt-1 flex flex-wrap gap-1">
                                                        {f.evidence_refs.map((ref) => (
                                                            <span
                                                                key={ref}
                                                                className="rounded bg-white/10 px-1 py-0.5 font-mono text-[10px] text-white/60"
                                                            >
                                                                {ref}
                                                            </span>
                                                        ))}
                                                    </div>
                                                )}
                                            </div>
                                        ))}
                                    </section>
                                )}

                                {/* Suggested commands */}
                                {result.commands.length > 0 && (
                                    <section className="flex flex-col gap-2">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t(
                                                "pages.desk.diagnose.commands",
                                            )}
                                        </h3>
                                        {result.commands.map((c, i) => (
                                            <div
                                                key={i}
                                                className="rounded-md border border-white/10 bg-white/5 p-2"
                                            >
                                                <div className="flex items-center justify-between gap-2">
                                                    <span className="flex items-center gap-1 text-[10px] uppercase text-white/50">
                                                        <TerminalIcon className="h-3 w-3" />
                                                        {c.shell}
                                                    </span>
                                                    <Badge
                                                        variant="outline"
                                                        className={riskClass(c.risk)}
                                                    >
                                                        {t(
                                                            `pages.desk.diagnose.risk.${c.risk}`,
                                                            c.risk,
                                                        )}
                                                    </Badge>
                                                </div>
                                                <pre className="mt-1 overflow-x-auto whitespace-pre-wrap break-all rounded bg-black/40 p-1.5 font-mono text-xs text-green-300">
                                                    {c.command}
                                                </pre>
                                                <p className="mt-1 text-xs text-white/60">
                                                    {c.purpose}
                                                </p>
                                                {exec && (
                                                    <ExecRow
                                                        index={i}
                                                        command={c}
                                                        exec={exec}
                                                    />
                                                )}
                                            </div>
                                        ))}
                                        <p className="text-[10px] text-white/40">
                                            {exec
                                                ? t(
                                                      "pages.desk.diagnose.execNote",
                                                  )
                                                : t(
                                                      "pages.desk.diagnose.suggestOnly",
                                                  )}
                                        </p>
                                    </section>
                                )}

                                {/* Next steps */}
                                {result.next_steps.length > 0 && (
                                    <section className="flex flex-col gap-2">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t("pages.desk.diagnose.nextSteps")}
                                        </h3>
                                        <ul className="list-disc pl-5 text-xs text-white/80">
                                            {result.next_steps.map((s, i) => (
                                                <li key={i}>{s}</li>
                                            ))}
                                        </ul>
                                    </section>
                                )}

                                {/* Missing info */}
                                {result.missing_info.length > 0 && (
                                    <section className="flex flex-col gap-2">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t("pages.desk.diagnose.missingInfo")}
                                        </h3>
                                        <ul className="list-disc pl-5 text-xs text-white/60">
                                            {result.missing_info.map((s, i) => (
                                                <li key={i}>{s}</li>
                                            ))}
                                        </ul>
                                    </section>
                                )}

                                {/* Data collected */}
                                {result.collected.length > 0 && (
                                    <section className="flex flex-col gap-2">
                                        <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                            {t("pages.desk.diagnose.collected")}
                                        </h3>
                                        <div className="flex flex-wrap gap-1">
                                            {result.collected.map((cap) => (
                                                <span
                                                    key={cap}
                                                    className="rounded bg-white/10 px-1.5 py-0.5 font-mono text-[10px] text-white/60"
                                                >
                                                    {cap}
                                                </span>
                                            ))}
                                        </div>
                                    </section>
                                )}
                            </>
                        ) : state.answer !== null ? (
                            // Agentic loop final answer (free text, not a
                            // structured Diagnosis) plus the tool timeline.
                            <div className="flex flex-col gap-4">
                                <ToolTimeline tools={state.tools} />
                                <section className="flex flex-col gap-2">
                                    <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-400">
                                        {t("pages.desk.diagnose.answer")}
                                    </h3>
                                    <p className="whitespace-pre-wrap text-sm text-white/90">
                                        {state.answer || streamingSummary}
                                    </p>
                                </section>
                            </div>
                        ) : (
                            // Handed off mid-stream with whatever was gathered.
                            <div className="flex flex-col gap-2">
                                <ToolTimeline tools={state.tools} />
                                {streamingSummary ? (
                                    <p className="whitespace-pre-wrap text-sm text-white/90">
                                        {streamingSummary}
                                    </p>
                                ) : (
                                    <p className="text-sm text-white/60">
                                        {t(
                                            "pages.desk.diagnose.handedOff",
                                        )}
                                    </p>
                                )}
                            </div>
                        )}
                        {/* Continue the conversation with another question. */}
                        <FollowUpComposer onSubmit={askFollowUp} />
                    </div>
                )}
            </div>

            {/* Footer actions */}
            <div className="flex items-center gap-2 border-t border-white/15 px-4 py-3">
                {(state.phase === "running" || state.phase === "done") && (
                    <Button
                        size="sm"
                        variant="secondary"
                        className="flex-1"
                        onClick={onHandoff}
                    >
                        <UserCog className="mr-1 h-3.5 w-3.5" />
                        {t("pages.desk.diagnose.handoff")}
                    </Button>
                )}
                {state.phase !== "idle" && (
                    <Button size="sm" variant="ghost" className="flex-1" onClick={onReset}>
                        {t("pages.desk.diagnose.newDiagnosis")}
                    </Button>
                )}
            </div>
        </div>
    )
}

export default DiagnosePanel
