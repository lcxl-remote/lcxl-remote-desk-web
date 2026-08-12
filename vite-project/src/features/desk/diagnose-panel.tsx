import {
    type PointerEvent as ReactPointerEvent,
    useEffect,
    useLayoutEffect,
    useRef,
    useState,
} from "react"
import { useTranslation } from "react-i18next"
import {
    AlertCircle,
    ArrowDown,
    History,
    Loader2,
    Stethoscope,
    Terminal as TerminalIcon,
    X,
} from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import { AiGeneratedMark } from "@/components/ai-generated-mark"
import { MarkdownContent } from "@/components/markdown-content"
import { agentErrorMessage, contentRetractionMessage } from "@/lib/agent-error-i18n"
import { useFollowLatest } from "@/hooks/use-follow-latest"
import {
    extractStreamingSummary,
    type DiagnoseState,
    type DiagnoseSessionSummary,
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
    historySessions?: DiagnoseSessionSummary[]
    historyLoading?: boolean
    historyError?: boolean
    onRefreshHistory?: () => void
    onRestoreSession?: (session: DiagnoseSessionSummary) => void
    canContinue?: boolean
}

type ResizeDirection = "width" | "height" | "both"

const DEFAULT_PANEL_WIDTH = 380
const DEFAULT_PANEL_HEIGHT = 720
const MIN_PANEL_WIDTH = 320
const MIN_PANEL_HEIGHT = 280
const PANEL_VIEWPORT_GAP = 16

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
    historySessions = [],
    historyLoading = false,
    historyError = false,
    onRefreshHistory,
    onRestoreSession,
    canContinue = true,
}: DiagnosePanelProps) {
    const { t, i18n } = useTranslation()
    const [question, setQuestion] = useState("")
    const [includeScreen, setIncludeScreen] = useState(false)
    // The manager-selected agent model, or null when the selector is hidden
    // (open-source signal) — in which case no `model_id` is sent.
    const [modelId, setModelId] = useState<number | null>(null)
    const [selectedModelSupportsImage, setSelectedModelSupportsImage] = useState<boolean | null>(
        null,
    )
    const [showHistory, setShowHistory] = useState(false)
    const [signalSupportsImage, setSignalSupportsImage] = useState<boolean | null>(null)
    const panelRef = useRef<HTMLDivElement>(null)
    const [panelSize, setPanelSize] = useState({
        width: DEFAULT_PANEL_WIDTH,
        height: DEFAULT_PANEL_HEIGHT,
    })
    const [panelOffset, setPanelOffset] = useState({ x: 0, y: 0 })
    const dragStartRef = useRef<{
        x: number
        y: number
        offsetX: number
        offsetY: number
        left: number
        top: number
        right: number
        bottom: number
    } | null>(null)
    const resizeStartRef = useRef<{
        direction: ResizeDirection
        x: number
        y: number
        width: number
        height: number
    } | null>(null)
    const {
        scrollRef,
        onScroll,
        showJumpToLatest,
        jumpToLatest,
    } = useFollowLatest(!showHistory && state.phase !== "idle")

    useEffect(() => {
        let cancelled = false
        void fetch("/api/model/provider", {
            credentials: "include",
            headers: { Accept: "application/json" },
        })
            .then(async (response) => {
                if (!response.ok) return
                const body = (await response.json()) as {
                    success?: boolean
                    data?: { supports_image_input?: boolean } | null
                }
                if (!cancelled && body.success !== false && body.data) {
                    setSignalSupportsImage(body.data.supports_image_input === true)
                }
            })
            .catch(() => undefined)
        return () => {
            cancelled = true
        }
    }, [])

    const supportsImage = selectedModelSupportsImage ?? signalSupportsImage

    const toggleHistory = () => {
        setShowHistory((current) => {
            if (!current) onRefreshHistory?.()
            return !current
        })
    }

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

    const panelBounds = () => {
        const rect = panelRef.current?.getBoundingClientRect()
        if (rect && rect.width > 0 && rect.height > 0) return rect

        const right = window.innerWidth - PANEL_VIEWPORT_GAP + panelOffset.x
        const top = PANEL_VIEWPORT_GAP + panelOffset.y
        return {
            left: right - panelSize.width,
            top,
            right,
            bottom: top + panelSize.height,
            width: panelSize.width,
            height: panelSize.height,
        }
    }

    const parentBounds = () => {
        const rect = panelRef.current?.parentElement?.getBoundingClientRect()
        if (rect && rect.width > 0 && rect.height > 0) return rect
        return {
            left: 0,
            top: 0,
            right: window.innerWidth,
            bottom: window.innerHeight,
            width: window.innerWidth,
            height: window.innerHeight,
        }
    }

    const startDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
        if ((event.target as Element).closest("button")) return
        const rect = panelBounds()
        dragStartRef.current = {
            x: event.clientX,
            y: event.clientY,
            offsetX: panelOffset.x,
            offsetY: panelOffset.y,
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
        event.currentTarget.setPointerCapture(event.pointerId)
        event.preventDefault()
    }

    const continueDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
        const start = dragStartRef.current
        if (!start) return
        const parent = parentBounds()
        const requestedX = event.clientX - start.x
        const requestedY = event.clientY - start.y
        const deltaX = Math.min(
            parent.right - PANEL_VIEWPORT_GAP - start.right,
            Math.max(
                parent.left + PANEL_VIEWPORT_GAP - start.left,
                requestedX,
            ),
        )
        const deltaY = Math.min(
            parent.bottom - PANEL_VIEWPORT_GAP - start.bottom,
            Math.max(
                parent.top + PANEL_VIEWPORT_GAP - start.top,
                requestedY,
            ),
        )
        setPanelOffset({
            x: start.offsetX + deltaX,
            y: start.offsetY + deltaY,
        })
    }

    const finishDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
        dragStartRef.current = null
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
            event.currentTarget.releasePointerCapture(event.pointerId)
        }
    }

    const startResize = (
        direction: ResizeDirection,
        event: ReactPointerEvent<HTMLDivElement>,
    ) => {
        const rect = panelRef.current?.getBoundingClientRect()
        resizeStartRef.current = {
            direction,
            x: event.clientX,
            y: event.clientY,
            width: rect && rect.width > 0 ? rect.width : panelSize.width,
            height: rect && rect.height > 0 ? rect.height : panelSize.height,
        }
        event.currentTarget.setPointerCapture(event.pointerId)
        event.preventDefault()
    }

    const continueResize = (event: ReactPointerEvent<HTMLDivElement>) => {
        const start = resizeStartRef.current
        if (!start) return

        const parent = parentBounds()
        const panel = panelBounds()
        const maxWidth = Math.max(
            1,
            panel.right - parent.left - PANEL_VIEWPORT_GAP,
        )
        const maxHeight = Math.max(
            1,
            parent.bottom - PANEL_VIEWPORT_GAP - panel.top,
        )
        const minWidth = Math.min(MIN_PANEL_WIDTH, maxWidth)
        const minHeight = Math.min(MIN_PANEL_HEIGHT, maxHeight)

        setPanelSize((current) => ({
            width:
                start.direction === "height"
                    ? current.width
                    : Math.min(
                          maxWidth,
                          Math.max(
                              minWidth,
                              start.width + start.x - event.clientX,
                          ),
                      ),
            height:
                start.direction === "width"
                    ? current.height
                    : Math.min(
                          maxHeight,
                          Math.max(
                              minHeight,
                              start.height + event.clientY - start.y,
                          ),
                      ),
        }))
    }

    const finishResize = (event: ReactPointerEvent<HTMLDivElement>) => {
        resizeStartRef.current = null
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
            event.currentTarget.releasePointerCapture(event.pointerId)
        }
    }

    useLayoutEffect(() => {
        const panel = panelRef.current
        const parent = panel?.parentElement
        if (!panel || !parent) return

        const keepInsideParent = () => {
            const panelRect = panel.getBoundingClientRect()
            const parentRect = parent.getBoundingClientRect()
            if (
                panelRect.width <= 0 ||
                panelRect.height <= 0 ||
                parentRect.width <= 0 ||
                parentRect.height <= 0
            ) {
                return
            }

            let deltaX = 0
            let deltaY = 0
            if (panelRect.left < parentRect.left + PANEL_VIEWPORT_GAP) {
                deltaX = parentRect.left + PANEL_VIEWPORT_GAP - panelRect.left
            } else if (panelRect.right > parentRect.right - PANEL_VIEWPORT_GAP) {
                deltaX = parentRect.right - PANEL_VIEWPORT_GAP - panelRect.right
            }
            if (panelRect.top < parentRect.top + PANEL_VIEWPORT_GAP) {
                deltaY = parentRect.top + PANEL_VIEWPORT_GAP - panelRect.top
            } else if (panelRect.bottom > parentRect.bottom - PANEL_VIEWPORT_GAP) {
                deltaY = parentRect.bottom - PANEL_VIEWPORT_GAP - panelRect.bottom
            }

            if (deltaX !== 0 || deltaY !== 0) {
                setPanelOffset((current) => ({
                    x: current.x + deltaX,
                    y: current.y + deltaY,
                }))
            }
        }

        keepInsideParent()
        if (typeof ResizeObserver === "undefined") return
        const observer = new ResizeObserver(keepInsideParent)
        observer.observe(parent)
        observer.observe(panel)
        return () => observer.disconnect()
    }, [panelSize.height, panelSize.width])

    return (
        <div
            ref={panelRef}
            className="absolute top-4 right-4 z-50 flex min-h-[min(280px,calc(100%-2rem))] min-w-[min(320px,calc(100%-2rem))] max-w-[calc(100%-2rem)] max-h-[calc(100%-2rem)] flex-col overflow-hidden rounded-lg border border-white/20 bg-black/70 text-white shadow-xl backdrop-blur-md select-text"
            style={{
                width: panelSize.width,
                height: panelSize.height,
                transform: `translate(${panelOffset.x}px, ${panelOffset.y}px)`,
            }}
        >
            <div
                role="separator"
                aria-orientation="vertical"
                aria-label={t("pages.desk.diagnose.resizeWidth")}
                className="absolute inset-y-0 left-0 z-[2] w-1.5 cursor-col-resize touch-none bg-transparent transition-colors hover:bg-white/30"
                onPointerDown={(event) => startResize("width", event)}
                onPointerMove={continueResize}
                onPointerUp={finishResize}
                onPointerCancel={finishResize}
            />
            <div
                role="separator"
                aria-orientation="horizontal"
                aria-label={t("pages.desk.diagnose.resizeHeight")}
                className="absolute inset-x-0 bottom-0 z-[2] h-1.5 cursor-row-resize touch-none bg-transparent transition-colors hover:bg-white/30"
                onPointerDown={(event) => startResize("height", event)}
                onPointerMove={continueResize}
                onPointerUp={finishResize}
                onPointerCancel={finishResize}
            />
            <div
                role="separator"
                aria-label={t("pages.desk.diagnose.resizeBoth")}
                className="absolute bottom-0 left-0 z-[3] h-4 w-4 cursor-nesw-resize touch-none rounded-tr bg-white/20 transition-colors hover:bg-white/50"
                onPointerDown={(event) => startResize("both", event)}
                onPointerMove={continueResize}
                onPointerUp={finishResize}
                onPointerCancel={finishResize}
            />
            {/* Header */}
            <div
                data-testid="diagnose-drag-handle"
                className="flex cursor-grab touch-none select-none items-center justify-between border-b border-white/15 px-4 py-3 active:cursor-grabbing"
                onPointerDown={startDrag}
                onPointerMove={continueDrag}
                onPointerUp={finishDrag}
                onPointerCancel={finishDrag}
            >
                <div className="flex items-center gap-2 text-sm font-bold text-white/90">
                    <Stethoscope className="h-4 w-4" style={{ stroke: "url(#ai-rainbow-gradient)" }} />
                    {t("pages.desk.diagnose.title")}
                </div>
                <button
                    onClick={onClose}
                    className="cursor-pointer text-gray-400 transition-colors hover:text-white"
                    aria-label={t("pages.desk.diagnose.close")}
                >
                    <X className="h-4 w-4" />
                </button>
            </div>

            <div className="relative min-h-0 flex-1">
                <div
                    ref={scrollRef}
                    onScroll={onScroll}
                    data-testid="diagnose-scroll-area"
                    className="h-full overflow-y-auto px-4 py-3 text-sm"
                >
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
                {showHistory && (
                    <div className="flex flex-col gap-2">
                        <div className="flex items-center justify-between">
                            <h3 className="text-xs font-semibold uppercase tracking-wide text-gray-300">
                                {t("pages.desk.diagnose.historyTitle")}
                            </h3>
                            <Button
                                type="button"
                                size="sm"
                                variant="ghost"
                                className="h-7 px-2 text-xs"
                                onClick={() => onRefreshHistory?.()}
                            >
                                {t("pages.desk.diagnose.historyRefresh")}
                            </Button>
                        </div>
                        {historyLoading ? (
                            <div className="flex items-center gap-2 py-4 text-xs text-gray-400">
                                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                {t("pages.desk.diagnose.historyLoading")}
                            </div>
                        ) : historyError ? (
                            <div className="py-4 text-xs text-red-300">
                                {t("pages.desk.diagnose.historyError")}
                            </div>
                        ) : historySessions.length === 0 ? (
                            <div className="py-4 text-xs text-gray-400">
                                {t("pages.desk.diagnose.historyEmpty")}
                            </div>
                        ) : (
                            historySessions.map((session) => (
                                <button
                                    type="button"
                                    key={session.sessionId}
                                    disabled={session.active}
                                    className="rounded-md border border-white/15 bg-white/5 p-2 text-left transition-colors hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-50"
                                    onClick={() => {
                                        onRestoreSession?.(session)
                                        setShowHistory(false)
                                    }}
                                >
                                    <div className="line-clamp-2 text-xs font-medium text-white/90">
                                        {session.firstQuestion ||
                                            t("pages.desk.diagnose.historyUntitled")}
                                    </div>
                                    <div className="mt-1 flex items-center justify-between gap-2 text-[10px] text-gray-400">
                                        <span>
                                            {new Intl.DateTimeFormat(i18n.language, {
                                                dateStyle: "medium",
                                                timeStyle: "short",
                                            }).format(new Date(session.updatedAt))}
                                        </span>
                                        <span>
                                            {session.active
                                                ? t("pages.desk.diagnose.historyActive")
                                                : t("pages.desk.diagnose.historyOpen")}
                                        </span>
                                    </div>
                                </button>
                            ))
                        )}
                    </div>
                )}
                {!showHistory && (
                    <>
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
                                disabled={supportsImage === false}
                                onChange={(e) => setIncludeScreen(e.target.checked)}
                                className="h-3.5 w-3.5"
                            />
                            {t("pages.desk.diagnose.includeScreen")}
                        </label>
                        {supportsImage === false && (
                            <p className="text-xs text-amber-300" role="note">
                                {t("pages.desk.diagnose.imageModelRequired")}
                            </p>
                        )}

                        {/* Manager-only agent-model picker; renders nothing against
                            an open-source signal server, leaving the flow unchanged. */}
                        <ModelSelector
                            role="agent"
                            orgId={orgId}
                            onChange={setModelId}
                            onModelChange={(model) => {
                                const supports = model?.supports_image_input ?? null
                                setSelectedModelSupportsImage(supports)
                                if (supports === false) setIncludeScreen(false)
                            }}
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
                                {state.retractionReason
                                    ? contentRetractionMessage(t, state.retractionReason)
                                    : agentErrorMessage(
                                          t,
                                          state.errorCode,
                                          state.error,
                                          state.error ?? "",
                                      )}
                            </span>
                        </div>
                        {/* A failed turn is settled, so a follow-up may continue the
                            same conversation (the backend allows re-claiming it). */}
                        {canContinue && <FollowUpComposer onSubmit={askFollowUp} />}
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
                        {canContinue ? (
                            <FollowUpComposer onSubmit={askFollowUp} />
                        ) : (
                            <p className="rounded-md border border-amber-500/30 bg-amber-500/10 p-2 text-xs text-amber-200">
                                {t("pages.desk.diagnose.historyReadOnly")}
                            </p>
                        )}
                    </div>
                )}
                    </>
                )}
                </div>
                {showJumpToLatest && (
                    <button
                        type="button"
                        onClick={jumpToLatest}
                        className="absolute right-3 bottom-3 flex h-9 w-9 items-center justify-center rounded-full border border-white/25 bg-black/85 text-white shadow-lg transition-colors hover:bg-black focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-white/70"
                        aria-label={t("pages.desk.diagnose.scrollToLatest")}
                        title={t("pages.desk.diagnose.scrollToLatest")}
                    >
                        <ArrowDown className="h-4 w-4" />
                    </button>
                )}
            </div>

            {/* Footer actions */}
            <div className="flex items-center gap-2 border-t border-white/15 px-4 py-3">
                <Button
                    size="sm"
                    variant="ghost"
                    className="flex-1"
                    onClick={toggleHistory}
                    disabled={state.phase === "running"}
                >
                    <History className="mr-1.5 h-3.5 w-3.5" />
                    {showHistory
                        ? t("pages.desk.diagnose.historyBack")
                        : t("pages.desk.diagnose.history")}
                </Button>
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
