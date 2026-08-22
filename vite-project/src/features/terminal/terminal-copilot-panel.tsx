import { type PointerEvent as ReactPointerEvent, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
    Sparkles,
    X,
    Loader2,
    AlertCircle,
    Wrench,
    ClipboardCopy,
    CornerDownLeft,
    Ban,
    Play,
    ArrowDown,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { AiGeneratedMark } from '@/components/ai-generated-mark';
import { MarkdownContent } from '@/components/markdown-content';
import { agentErrorMessage, contentRetractionMessage } from '@/lib/agent-error-i18n';
import type {
    CommandSuggestion,
    CopilotState,
    RiskLevel,
    TerminalCopilotMode,
} from './use-terminal-copilot';
import { ExecLifecycle } from '../exec/exec-lifecycle';
import type { ExecEntry, ExecRequestInput } from '../exec/use-confirm-exec';
import { ModelSelector } from '../desk/model-selector';
import { useFollowLatest } from '@/hooks/use-follow-latest';

/** Confirmed-execution controls, shared with the diagnose panel via
 *  `useConfirmExec`. Omitted when the copilot is rendered suggest-only. */
export type CopilotExecControls = {
    entries: Record<number, ExecEntry>;
    requestPreview: (rowIndex: number, input: ExecRequestInput) => void;
    approve: (rowIndex: number) => void;
    reject: (rowIndex: number) => void;
    cancel: (rowIndex: number) => void;
    dismiss: (rowIndex: number) => void;
};

/** Per-turn stride for the exec entry index, so suggestions from different turns
 *  in the multi-turn log never collide on a shared `useConfirmExec` row key
 *  (`turnIndex * stride + rowIndex`). A turn never proposes anywhere near this
 *  many commands. */
const COPILOT_INDEX_STRIDE = 100;
const COPILOT_MIN_WIDTH = 280;
const COPILOT_MAX_WIDTH = 720;

/** Map a suggested-command risk level to a badge colour (mirrors the diagnose
 *  panel so the two AI surfaces read consistently). */
function riskClass(risk: RiskLevel): string {
    switch (risk) {
        case 'low':
            return 'bg-green-500/20 text-green-300 border-green-500/40';
        case 'medium':
            return 'bg-yellow-500/20 text-yellow-300 border-yellow-500/40';
        case 'high':
            return 'bg-orange-500/20 text-orange-300 border-orange-500/40';
        default:
            return 'bg-red-500/20 text-red-300 border-red-500/40';
    }
}

type SuggestionRowProps = {
    index: number;
    suggestion: CommandSuggestion;
    onFill: (command: string) => void;
    /** Confirmed-execution controls; omitted in suggest-only contexts. */
    exec?: CopilotExecControls;
};

/**
 * One proposed command. Actions are gated on the server-computed `decision`,
 * never a model-self-reported field (suggest-only invariant):
 * - `blocked`: shown as a hard-denied warning with no actions and no injection.
 * - `not_executable` / `confirm_required`: Fill (type it into the shell without a
 *   trailing Enter — the operator presses Enter themselves) and Copy.
 * - `confirm_required` additionally offers Run, which the operator chooses to
 *   promote the suggestion into the host's sealed confirm-exec chain (the host
 *   re-classifies the command server-side and an explicit preview/approval is
 *   still required). Run only appears when `exec` controls are wired and the
 *   device's execution ceiling allows it; otherwise the host returns a
 *   non-executable preview with guidance.
 */
function SuggestionRow({ index, suggestion, onFill, exec }: SuggestionRowProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const blocked = suggestion.decision === 'blocked';
    const canRun = !!exec && suggestion.decision === 'confirm_required';
    const entry = exec?.entries[index];
    // A non-executable preview that is not a hard block is typically the device
    // execution ceiling being too low; guide the operator to raise it.
    const modeBlocked =
        entry?.phase === 'error' &&
        !!entry.preview &&
        !entry.preview.executable &&
        !entry.preview.blocked_reason;

    const copy = async () => {
        try {
            await navigator.clipboard.writeText(suggestion.command);
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1500);
        } catch {
            // Clipboard may be unavailable (insecure context); ignore silently.
        }
    };

    return (
        <div className="rounded-md border border-border/60 bg-background/40 p-2 text-sm">
            <div className="flex items-start justify-between gap-2">
                <code className="break-all font-mono text-xs text-foreground">
                    {suggestion.command}
                </code>
                <Badge variant="outline" className={riskClass(suggestion.risk)}>
                    {t(`pages.deskTerminal.copilot.risk.${suggestion.risk}`, suggestion.risk)}
                </Badge>
            </div>
            {suggestion.note && (
                <p className="mt-1 text-xs text-muted-foreground">{suggestion.note}</p>
            )}
            {blocked ? (
                <div className="mt-2 flex items-center gap-1 text-xs text-red-400">
                    <Ban className="h-3.5 w-3.5" />
                    {t(
                        'pages.deskTerminal.copilot.blockedHint',
                    )}
                </div>
            ) : (
                <>
                    <div className="mt-2 flex flex-wrap gap-2">
                        <Button
                            size="sm"
                            variant="secondary"
                            onClick={() => onFill(suggestion.command)}
                        >
                            <CornerDownLeft className="mr-1 h-3.5 w-3.5" />
                            {t('pages.deskTerminal.copilot.fill')}
                        </Button>
                        <Button size="sm" variant="ghost" onClick={copy}>
                            <ClipboardCopy className="mr-1 h-3.5 w-3.5" />
                            {copied
                                ? t('pages.deskTerminal.copilot.copied')
                                : t('pages.deskTerminal.copilot.copy')}
                        </Button>
                        {canRun && !entry && (
                            <Button
                                size="sm"
                                variant="secondary"
                                onClick={() =>
                                    exec!.requestPreview(index, {
                                        shell: suggestion.shell,
                                        command: suggestion.command,
                                        cwd: suggestion.cwd ?? null,
                                        reason: suggestion.note,
                                    })
                                }
                            >
                                <Play className="mr-1 h-3.5 w-3.5" />
                                {t('pages.deskTerminal.copilot.run')}
                            </Button>
                        )}
                    </div>
                    {exec && entry && (
                        <ExecLifecycle
                            entry={entry}
                            onApprove={() => exec.approve(index)}
                            onReject={() => exec.reject(index)}
                            onCancel={() => exec.cancel(index)}
                            onDismiss={() => exec.dismiss(index)}
                        />
                    )}
                    {modeBlocked && (
                        <p className="mt-1 text-[10px] text-muted-foreground">
                            {t('pages.deskTerminal.copilot.execGuide')}
                        </p>
                    )}
                </>
            )}
        </div>
    );
}

export type TerminalCopilotPanelProps = {
    state: CopilotState;
    /** `modelId` is the manager-selected agent model, or null when the selector is
     *  hidden (open-source signal) — then no `model_id` is sent. */
    onAsk: (mode: TerminalCopilotMode, question: string, modelId: number | null) => void;
    onReset: () => void;
    onClose: () => void;
    /** Inject the command into the shell input without a trailing Enter. */
    onFill: (command: string) => void;
    /** Confirmed-execution controls; omitted in suggest-only contexts. */
    exec?: CopilotExecControls;
    /** Manager-only active-organization id, threaded to the model selector so the
     *  copilot catalog and preference are org-scoped. Undefined for the personal
     *  view / open-source control end. */
    orgId?: number;
};

/**
 * The in-terminal AI copilot side panel. Two scenarios share one surface:
 * `how_to` (describe an intent → command suggestions) and `explain_error`
 * (explain the latest error → a fix). Streaming progress and the final
 * suggestions render inline; every action is suggest-only.
 */
export function TerminalCopilotPanel({
    state,
    onAsk,
    onReset,
    onClose,
    onFill,
    exec,
    orgId,
}: TerminalCopilotPanelProps) {
    const { t } = useTranslation();
    const [mode, setMode] = useState<TerminalCopilotMode>('how_to');
    const [question, setQuestion] = useState('');
    // The manager-selected agent model, or null when the selector is hidden.
    const [modelId, setModelId] = useState<number | null>(null);
    const [panelWidth, setPanelWidth] = useState(320);
    const resizeStartRef = useRef<{ x: number; width: number } | null>(null);

    const running = state.phase === 'running';
    const streamedText =
        state.committedText + (running ? state.partialText : '');

    // Follow streaming output only while the reader remains at the bottom. If
    // they scroll up to inspect an earlier turn, preserve that position and
    // offer an explicit jump back to the latest content.
    const {
        scrollRef,
        onScroll,
        showJumpToLatest,
        jumpToLatest,
    } = useFollowLatest();

    const onResizePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
        resizeStartRef.current = { x: event.clientX, width: panelWidth };
        event.currentTarget.setPointerCapture(event.pointerId);
        event.preventDefault();
    };

    const onResizePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
        const start = resizeStartRef.current;
        if (!start) return;
        const containerLimit = Math.max(
            COPILOT_MIN_WIDTH,
            Math.floor(window.innerWidth * 0.7),
        );
        setPanelWidth(
            Math.min(
                COPILOT_MAX_WIDTH,
                containerLimit,
                Math.max(COPILOT_MIN_WIDTH, start.width + start.x - event.clientX),
            ),
        );
    };

    const onResizePointerUp = (event: ReactPointerEvent<HTMLDivElement>) => {
        resizeStartRef.current = null;
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
            event.currentTarget.releasePointerCapture(event.pointerId);
        }
    };

    const submit = () => {
        if (running) return;
        if (mode === 'how_to' && !question.trim()) return;
        onAsk(mode, question.trim(), modelId);
        setQuestion('');
    };

    return (
        <div
            className="relative flex h-full shrink-0 flex-col overflow-hidden border-l border-border bg-card text-card-foreground"
            style={{ width: panelWidth }}
        >
            <div
                role="separator"
                aria-orientation="vertical"
                aria-label={t('pages.deskTerminal.copilot.resizePanel')}
                className="absolute inset-y-0 left-0 z-20 w-1 cursor-col-resize touch-none bg-transparent transition-colors hover:bg-primary/40"
                onPointerDown={onResizePointerDown}
                onPointerMove={onResizePointerMove}
                onPointerUp={onResizePointerUp}
                onPointerCancel={onResizePointerUp}
            />
            <div className="flex items-center justify-between border-b border-border px-3 py-2">
                <div className="flex items-center gap-2 font-medium">
                    <Sparkles className="h-4 w-4 text-primary" />
                    {t('pages.deskTerminal.copilot.title')}
                </div>
                <Button variant="ghost" size="icon" className="h-7 w-7" onClick={onClose}>
                    <X className="h-4 w-4" />
                </Button>
            </div>

            <div className="relative min-h-0 flex-1">
                <div
                    ref={scrollRef}
                    onScroll={onScroll}
                    data-testid="terminal-copilot-scroll-area"
                    className="h-full space-y-4 overflow-y-auto p-3"
                >
                {/* AI interaction disclosure: informs the user, from the first interaction
                    and for every session, that they are interacting with an AI assistant.
                    Standing element at the top of the log (never a one-time, dismissible
                    banner) so it is always present and clearly distinguishable. Distinct from
                    the aiDisclaimer accuracy caveat in the footer. */}
                <p
                    role="note"
                    className="rounded-md border border-border bg-muted/50 px-2 py-1 text-xs font-medium text-foreground"
                >
                    {t('pages.deskTerminal.copilot.aiIdentityNotice')}
                </p>
                {state.turns.map((turn, turnIndex) => {
                    const isLast = turnIndex === state.turns.length - 1;
                    return (
                        <div key={turnIndex} className="space-y-2">
                            <div className="flex justify-end">
                                <div className="max-w-[90%] whitespace-pre-wrap break-words rounded-md bg-primary/10 px-2 py-1 text-sm text-foreground">
                                    {turn.question ||
                                        t('pages.deskTerminal.copilot.explainErrorTurn')}
                                </div>
                            </div>

                            {turn.answer ? (
                                <div className="space-y-3">
                                    {/* AI-generated marking (Art.50(2)) for the
                                        copilot answer. Driven by the answer being
                                        present, not by provenance (fail-closed). */}
                                    <AiGeneratedMark provenance={turn.provenance} />
                                    {turn.answer.explanation_md && (
                                        <MarkdownContent className="text-sm text-foreground">
                                            {turn.answer.explanation_md}
                                        </MarkdownContent>
                                    )}
                                    {turn.answer.suggestions.map((s, rowIndex) => (
                                        <SuggestionRow
                                            key={rowIndex}
                                            index={turnIndex * COPILOT_INDEX_STRIDE + rowIndex}
                                            suggestion={s}
                                            onFill={onFill}
                                            exec={exec}
                                        />
                                    ))}
                                    {turn.answer.suggestions.length === 0 && (
                                        <p className="text-xs text-muted-foreground">
                                            {t('pages.deskTerminal.copilot.noSuggestions')}
                                        </p>
                                    )}
                                </div>
                            ) : (
                                isLast && (
                                    <div className="space-y-2">
                                        {state.tools.length > 0 && (
                                            <div className="space-y-1">
                                                {state.tools.map((tool, i) => (
                                                    <div
                                                        key={i}
                                                        className="flex items-center gap-1 text-xs text-muted-foreground"
                                                    >
                                                        <Wrench className="h-3.5 w-3.5" />
                                                        {t(
                                                            'pages.deskTerminal.copilot.toolRan',
                                                            { name: tool.name },
                                                        )}
                                                    </div>
                                                ))}
                                            </div>
                                        )}

                                        {(running || state.phase === 'error') && streamedText && (
                                            <div className="space-y-2">
                                                {/* AI-generated marking (Art.50(2)) for the
                                                    streaming answer, shown as soon as text is
                                                    exposed — not only once the answer settles —
                                                    so first exposure is always marked. Driven by
                                                    the AI text being present (fail-closed); the
                                                    model is not yet known mid-stream. */}
                                                <AiGeneratedMark />
                                                <MarkdownContent className="text-sm text-muted-foreground">
                                                    {streamedText}
                                                </MarkdownContent>
                                            </div>
                                        )}

                                        {running &&
                                            !streamedText &&
                                            state.tools.length === 0 && (
                                                <div className="flex items-center gap-1 text-xs text-muted-foreground">
                                                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                                                    {t('pages.deskTerminal.copilot.thinking')}
                                                </div>
                                            )}

                                        {state.phase === 'error' && (
                                            <div className="flex items-start gap-2 rounded-md border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-300">
                                                <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                                                <span>
                                                    {state.retractionReason
                                                        ? contentRetractionMessage(t, state.retractionReason)
                                                        : agentErrorMessage(
                                                              t,
                                                              state.errorCode,
                                                              state.error,
                                                              t('pages.deskTerminal.copilot.error'),
                                                          )}
                                                </span>
                                            </div>
                                        )}
                                    </div>
                                )
                            )}
                        </div>
                    );
                })}
                </div>
                {showJumpToLatest && (
                    <Button
                        type="button"
                        variant="secondary"
                        size="icon"
                        className="absolute right-3 bottom-3 h-9 w-9 rounded-full shadow-lg"
                        onClick={jumpToLatest}
                        aria-label={t('pages.deskTerminal.copilot.scrollToLatest')}
                        title={t('pages.deskTerminal.copilot.scrollToLatest')}
                    >
                        <ArrowDown className="h-4 w-4" />
                    </Button>
                )}
            </div>

            <div className="border-t border-border p-3">
                {/* Standing reminder that AI output is fallible and should be verified. */}
                <p className="mb-2 text-xs text-muted-foreground">
                    {t('pages.deskTerminal.copilot.aiDisclaimer')}
                </p>
                <div className="mb-2 flex gap-1">
                    <Button
                        size="sm"
                        variant={mode === 'how_to' ? 'default' : 'outline'}
                        onClick={() => setMode('how_to')}
                    >
                        {t('pages.deskTerminal.copilot.modeHowTo')}
                    </Button>
                    <Button
                        size="sm"
                        variant={mode === 'explain_error' ? 'default' : 'outline'}
                        onClick={() => setMode('explain_error')}
                    >
                        {t('pages.deskTerminal.copilot.modeExplain')}
                    </Button>
                </div>
                {mode === 'how_to' ? (
                    <textarea
                        className="min-h-16 w-full resize-none rounded-md border border-input bg-background px-2 py-1 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                        placeholder={t(
                            'pages.deskTerminal.copilot.askPlaceholder',
                        )}
                        value={question}
                        onChange={(e) => setQuestion(e.target.value)}
                        onKeyDown={(e) => {
                            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) submit();
                        }}
                    />
                ) : (
                    <p className="text-xs text-muted-foreground">
                        {t(
                            'pages.deskTerminal.copilot.explainHint',
                        )}
                    </p>
                )}
                {/* Manager-only agent-model picker; renders nothing against an
                    open-source signal server, leaving the flow unchanged. */}
                <div className="mt-2">
                    <ModelSelector
                        role="agent"
                        orgId={orgId}
                        onChange={setModelId}
                        className="border-input bg-background text-foreground"
                    />
                </div>
                <div className="mt-2 flex gap-2">
                    <Button size="sm" onClick={submit} disabled={running}>
                        {running && <Loader2 className="mr-1 h-3.5 w-3.5 animate-spin" />}
                        {t('pages.deskTerminal.copilot.ask')}
                    </Button>
                    {(state.phase === 'done' || state.phase === 'error' || running) && (
                        <Button size="sm" variant="ghost" onClick={onReset}>
                            {t('pages.deskTerminal.copilot.reset')}
                        </Button>
                    )}
                </div>
            </div>
        </div>
    );
}
