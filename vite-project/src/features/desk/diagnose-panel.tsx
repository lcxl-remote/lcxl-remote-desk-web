import { useState } from "react"
import { useTranslation } from "react-i18next"
import {
    AlertCircle,
    Loader2,
    Stethoscope,
    Terminal as TerminalIcon,
    X,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { AiGeneratedMark } from "@/components/ai-generated-mark"
import { MarkdownContent } from "@/components/markdown-content"
import { agentErrorMessage } from "@/lib/agent-error-i18n"
import {
    extractStreamingSummary,
    type DiagnoseState,
    type DiagnoseStartOptions,
} from "./use-desk-diagnose"
import {
    AgenticExecApproval,
    ConversationTimeline,
    ConversationHistory,
    ExecRow,
    FollowUpComposer,
    QuestionBubble,
    confidenceClass,
    riskClass,
    type ExecControls,
} from "./diagnose-panel-sections"
import { ModelSelector } from "./model-selector"

type DiagnosePanelProps = {
    state: DiagnoseState
    onStart: (question: string, options: DiagnoseStartOptions) => void
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
    /** Stop the durable background command currently attached to the session. */
    onCancelBackgroundExec?: () => void
    /** Manager-only active-organization id, threaded to the model selector and the
     *  diagnose request. Undefined for the personal view / open-source control end,
     *  which keeps everything personal-scoped. */
    orgId?: number
}

export function DiagnosePanel({
    state,
    onStart,
    onReset,
    onClose,
    isConnected = true,
    exec,
    onApproveExec,
    onRejectExec,
    onCancelBackgroundExec,
    orgId,
}: DiagnosePanelProps) {
    const { t, i18n } = useTranslation()
    const [question, setQuestion] = useState("")
    const [includeScreen, setIncludeScreen] = useState(false)
    // The manager-selected agent model, or null when the selector is hidden
    // (open-source signal) — in which case no `model_id` is sent.
    const [modelId, setModelId] = useState<number | null>(null)

    const presets: string[] = [
        t("pages.desk.diagnose.presetCpu"),
        t("pages.desk.diagnose.presetPort"),
        t("pages.desk.diagnose.presetContainer"),
    ]

    const submit = (q: string) => {
        const trimmed = q.trim()
        if (!trimmed) return
        // Pass the current UI language so the AI answers in it.
        onStart(trimmed, { includeScreen, locale: i18n.language, modelId, orgId })
    }

    // A follow-up keeps the same conversation; the hook reuses the conversation
    // id so the model sees the prior turns.
    const askFollowUp = (q: string) =>
        onStart(q, { includeScreen, locale: i18n.language, modelId, orgId })

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
                {/* AI interaction disclosure: informs the user, from the first interaction
                    and for every session, that they are interacting with an AI assistant.
                    Kept as a standing element at the top of the panel (never a one-time,
                    dismissible banner) so it is always present and clearly distinguishable.
                    Distinct from aiDisclaimer below, which is an accuracy caveat rather than
                    an identity notice. Higher contrast than the disclaimer to stand out; uses
                    theme-independent light-on-dark colors to stay readable on the fixed dark
                    overlay under any app theme. */}
                <p
                    role="note"
                    className="mb-2 rounded-md border border-white/20 bg-white/10 px-2 py-1 text-xs font-medium text-white/90"
                >
                    {t("pages.desk.diagnose.aiIdentityNotice")}
                </p>
                {/* Standing reminder that AI output is fallible and should be verified.
                    The panel is a fixed dark overlay (bg-black/70 text-white) regardless
                    of the app theme, so use theme-independent light-on-dark colors here
                    instead of the themeable muted tokens, which turn dark-on-dark and
                    become unreadable under the light theme. */}
                <p className="mb-3 rounded-md bg-white/10 px-2 py-1 text-xs text-white/60">
                    {t("pages.desk.diagnose.aiDisclaimer")}
                </p>
                {/* Conversation transcript (prior settled turns) */}
                {(state.phase === "running" ||
                    state.phase === "done" ||
                    state.phase === "error") && (
                    <div className="mb-3 flex flex-col gap-3">
                        <ConversationHistory turns={state.history} />
                        {/* The live turn's own question, shown immediately rather
                            than only once the next turn freezes it into history. */}
                        {state.question && <QuestionBubble question={state.question} />}
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

                        {/* Manager-only agent-model picker; renders nothing against
                            an open-source signal server, leaving the flow unchanged. */}
                        <ModelSelector
                            role="agent"
                            orgId={orgId}
                            onChange={setModelId}
                            className="border-white/20 bg-white/10 text-white"
                        />

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
                        <ConversationTimeline items={state.timeline} />
                        {streamingSummary && (
                            <div className="flex flex-col gap-2">
                                {/* AI-generated marking (Art.50(2)) for the streaming
                                    answer, shown the moment text is exposed — not only
                                    after the turn settles — so first exposure is always
                                    marked. Driven by the AI text being present
                                    (fail-closed); the model is not yet known mid-stream. */}
                                <AiGeneratedMark className="self-start border-white/25 bg-white/10 text-white/80" />
                                <MarkdownContent className="text-sm text-white/90">
                                    {streamingSummary}
                                </MarkdownContent>
                            </div>
                        )}
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
                        <ConversationTimeline items={state.timeline} />
                        {streamingSummary && (
                            <div className="flex flex-col gap-2">
                                <AiGeneratedMark
                                    provenance={state.provenance}
                                    className="self-start border-white/25 bg-white/10 text-white/80"
                                />
                                <MarkdownContent className="text-sm text-white/90">
                                    {streamingSummary}
                                </MarkdownContent>
                            </div>
                        )}
                        <div className="flex items-start gap-2 text-sm text-red-300">
                            <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                            <span>
                                {agentErrorMessage(
                                    t,
                                    state.errorCode,
                                    state.error,
                                    state.error ?? "",
                                )}
                            </span>
                        </div>
                        {/* A failed turn is settled, so a follow-up may continue the
                            same conversation (the backend allows re-claiming it). */}
                        <FollowUpComposer onSubmit={askFollowUp} />
                    </div>
                )}

                {/* Result (done) */}
                {state.phase === "done" && (
                    <div className="flex flex-col gap-4">
                        {state.backgroundExecution && onCancelBackgroundExec && (
                            <section className="flex items-center justify-between gap-3 rounded-md border border-amber-500/40 bg-amber-500/10 p-2">
                                <div className="min-w-0">
                                    <div className="text-xs font-medium text-amber-200">
                                        {t("pages.desk.diagnose.backgroundRunning")}
                                    </div>
                                    <div className="truncate font-mono text-[10px] text-white/50">
                                        {state.backgroundExecution.executionGeneration}
                                    </div>
                                </div>
                                <Button
                                    type="button"
                                    size="sm"
                                    variant="destructive"
                                    className="h-7 shrink-0 text-xs"
                                    disabled={state.backgroundExecution.cancelRequested}
                                    onClick={onCancelBackgroundExec}
                                >
                                    {state.backgroundExecution.cancelRequested
                                        ? t("pages.desk.diagnose.backgroundCancelling")
                                        : t("pages.desk.diagnose.backgroundCancel")}
                                </Button>
                            </section>
                        )}
                        {/* AI-generated marking (Art.50(2)) for any AI-derived
                            result / answer. Shown by the presence of AI content,
                            not by provenance being set (fail-closed); provenance
                            enriches the tooltip with the model when known. */}
                        {(result || (state.timeline.length === 0 && streamingSummary)) && (
                            <AiGeneratedMark
                                provenance={state.provenance}
                                className="self-start border-white/25 bg-white/10 text-white/80"
                            />
                        )}
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
                                    <MarkdownContent className="text-sm text-white/90">
                                        {result.summary}
                                    </MarkdownContent>
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
                        ) : state.timeline.length > 0 ? (
                            <ConversationTimeline items={state.timeline} />
                        ) : (
                            streamingSummary ? (
                                <MarkdownContent className="text-sm text-white/90">
                                    {streamingSummary}
                                </MarkdownContent>
                            ) : null
                        )}
                        {/* Continue the conversation with another question. */}
                        <FollowUpComposer onSubmit={askFollowUp} />
                    </div>
                )}
            </div>

            {/* Footer actions */}
            <div className="flex items-center gap-2 border-t border-white/15 px-4 py-3">
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
