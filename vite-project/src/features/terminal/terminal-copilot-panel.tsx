import { useState } from 'react';
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
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import type {
    CommandSuggestion,
    CopilotState,
    RiskLevel,
    TerminalCopilotMode,
} from './use-terminal-copilot';

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
    suggestion: CommandSuggestion;
    onFill: (command: string) => void;
};

/**
 * One proposed command. Actions are gated on the server-computed `decision`,
 * never a model-self-reported field (suggest-only invariant):
 * - `blocked`: shown as a hard-denied warning with no actions and no injection.
 * - `not_executable` / `confirm_required`: Fill (type it into the shell without a
 *   trailing Enter — the operator presses Enter themselves) and Copy. Neither is
 *   automatically executed through the AI path.
 */
function SuggestionRow({ suggestion, onFill }: SuggestionRowProps) {
    const { t } = useTranslation();
    const [copied, setCopied] = useState(false);
    const blocked = suggestion.decision === 'blocked';

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
                <div className="mt-2 flex gap-2">
                    <Button size="sm" variant="secondary" onClick={() => onFill(suggestion.command)}>
                        <CornerDownLeft className="mr-1 h-3.5 w-3.5" />
                        {t('pages.deskTerminal.copilot.fill')}
                    </Button>
                    <Button size="sm" variant="ghost" onClick={copy}>
                        <ClipboardCopy className="mr-1 h-3.5 w-3.5" />
                        {copied
                            ? t('pages.deskTerminal.copilot.copied')
                            : t('pages.deskTerminal.copilot.copy')}
                    </Button>
                </div>
            )}
        </div>
    );
}

export type TerminalCopilotPanelProps = {
    state: CopilotState;
    onAsk: (mode: TerminalCopilotMode, question: string) => void;
    onReset: () => void;
    onClose: () => void;
    /** Inject the command into the shell input without a trailing Enter. */
    onFill: (command: string) => void;
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
}: TerminalCopilotPanelProps) {
    const { t } = useTranslation();
    const [mode, setMode] = useState<TerminalCopilotMode>('how_to');
    const [question, setQuestion] = useState('');

    const running = state.phase === 'running';

    const submit = () => {
        if (running) return;
        if (mode === 'how_to' && !question.trim()) return;
        onAsk(mode, question.trim());
    };

    return (
        <div className="flex h-full w-80 flex-col border-l border-border bg-card text-card-foreground">
            <div className="flex items-center justify-between border-b border-border px-3 py-2">
                <div className="flex items-center gap-2 font-medium">
                    <Sparkles className="h-4 w-4 text-primary" />
                    {t('pages.deskTerminal.copilot.title')}
                </div>
                <Button variant="ghost" size="icon" className="h-7 w-7" onClick={onClose}>
                    <X className="h-4 w-4" />
                </Button>
            </div>

            <div className="border-b border-border p-3">
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

            <div className="flex-1 space-y-3 overflow-y-auto p-3">
                {state.tools.length > 0 && (
                    <div className="space-y-1">
                        {state.tools.map((tool, i) => (
                            <div
                                key={i}
                                className="flex items-center gap-1 text-xs text-muted-foreground"
                            >
                                <Wrench className="h-3.5 w-3.5" />
                                {t('pages.deskTerminal.copilot.toolRan', {
                                    name: tool.name,
                                })}
                            </div>
                        ))}
                    </div>
                )}

                {running && state.partialText && (
                    <p className="whitespace-pre-wrap text-sm text-muted-foreground">
                        {state.partialText}
                    </p>
                )}

                {state.phase === 'error' && (
                    <div className="flex items-start gap-2 rounded-md border border-red-500/40 bg-red-500/10 p-2 text-sm text-red-300">
                        <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                        <span>{state.error}</span>
                    </div>
                )}

                {state.answer && (
                    <div className="space-y-3">
                        {state.answer.explanation_md && (
                            <p className="whitespace-pre-wrap text-sm text-foreground">
                                {state.answer.explanation_md}
                            </p>
                        )}
                        {state.answer.suggestions.map((s, i) => (
                            <SuggestionRow key={i} suggestion={s} onFill={onFill} />
                        ))}
                        {state.answer.suggestions.length === 0 && (
                            <p className="text-xs text-muted-foreground">
                                {t(
                                    'pages.deskTerminal.copilot.noSuggestions',
                                )}
                            </p>
                        )}
                    </div>
                )}
            </div>
        </div>
    );
}
