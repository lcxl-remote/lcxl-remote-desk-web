import { Button } from '@/components/ui/button';
import { useTranslation } from 'react-i18next';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import type { ContextUsageDto } from '@/services/types';

export type AssistantContextUsage = ContextUsageDto;

export function contextRequestBudget(usage: AssistantContextUsage | null) {
    const budget = usage?.requestBudget;
    if (!budget || !usage || ![budget.totalBytes, budget.systemPromptBytes, budget.toolDefinitionsBytes, budget.otherOverheadBytes]
        .every(value => Number.isSafeInteger(value) && value >= 0) || budget.totalBytes <= 0) return null;
    const overhead = budget.systemPromptBytes + budget.toolDefinitionsBytes + budget.otherOverheadBytes;
    return Number.isSafeInteger(overhead) && budget.totalBytes - overhead === usage.limitBytes && usage.limitBytes > 0 ? budget : null;
}

export function contextBudgetShare(bytes: number, total: number) {
    if (!Number.isSafeInteger(bytes) || !Number.isSafeInteger(total) || bytes < 0 || total <= 0 || bytes > total) return null;
    const percent = bytes / total * 100;
    return percent > 0 && percent < 0.1 ? 'small' : Math.min(bytes < total ? 99.9 : 100, Math.round(percent * 10) / 10);
}

export function contextMeterValues(usage: AssistantContextUsage | null, draft: string) {
    if (!usage || !Number.isSafeInteger(usage.usedBytes) || !Number.isSafeInteger(usage.limitBytes)
        || usage.usedBytes < 0 || usage.limitBytes <= 0
        || !['window', 'checkpoint_summary'].includes(usage.strategy)) return null;
    const text = draft.trim();
    const draftBytes = text ? new TextEncoder().encode(JSON.stringify({ role: 'user', text })).length : 0;
    return {
        percent: Math.min(100, Math.floor(usage.usedBytes / usage.limitBytes * 100)),
        remaining: Math.max(0, usage.limitBytes - usage.usedBytes),
        draftBytes,
    };
}

export function AssistantContextMeter({ usage, draft }: { usage: AssistantContextUsage | null; draft: string }) {
    const { t, i18n } = useTranslation();
    const values = contextMeterValues(usage, draft);
    const budget = contextRequestBudget(usage);
    const bytes = (n: number) => t('pages.aiAssistant.contextMeter.bytes', { value: new Intl.NumberFormat(i18n.language).format(n) });
    const share = (cost: number, total: number) => {
        const value = contextBudgetShare(cost, total);
        return value === 'small' ? t('pages.aiAssistant.contextMeter.shareSmall')
            : value == null ? '—' : t('pages.aiAssistant.contextMeter.share', { value: new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }).format(value) });
    };
    const label = t(values ? 'pages.aiAssistant.contextMeter.percent' : 'pages.aiAssistant.contextMeter.unknown', { percent: values?.percent });
    return <Popover><PopoverTrigger asChild>
        <Button variant="unstyled" type="button" aria-label={label} className="assistant-context-meter flex h-11 w-11 shrink-0 items-center justify-center rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
            <svg viewBox="0 0 40 40" className="h-10 w-10" aria-hidden="true">
                <circle cx="20" cy="20" r="17" fill="none" stroke="currentColor" strokeWidth="3" className="text-muted" />
                {values && <circle cx="20" cy="20" r="17" fill="none" stroke="currentColor" strokeWidth="3"
                    pathLength="100" strokeDasharray={`${values.percent} 100`} transform="rotate(-90 20 20)"
                    className={values.percent >= 90 ? 'text-amber-500' : 'text-primary'} />}
                <text x="20" y="20" dy=".35em" textAnchor="middle" fill="currentColor" fontSize="10">{values ? `${values.percent}%` : '—'}</text>
            </svg>
        </Button>
    </PopoverTrigger><PopoverContent side="top" collisionPadding={16} aria-label={t('pages.aiAssistant.contextMeter.title')} className="w-96 max-w-[calc(100vw-2rem)] max-h-[var(--radix-popover-content-available-height)] overflow-y-auto overscroll-contain space-y-3 p-3">
        <p className="font-medium">{t('pages.aiAssistant.contextMeter.title')}</p>
        {values && usage ? <>
            <section aria-label={t('pages.aiAssistant.contextMeter.requestTitle')} className="space-y-2">
                <p className="text-sm font-medium">{t('pages.aiAssistant.contextMeter.requestTitle')}</p>
                <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-2 text-sm">
                    <dt>{t('pages.aiAssistant.contextMeter.total')}</dt><dd className="text-right tabular-nums">{budget ? bytes(budget.totalBytes) : '—'}</dd>
                    {(['systemPromptBytes', 'toolDefinitionsBytes', 'otherOverheadBytes'] as const).map(key => <div key={key} className="contents">
                        <dt>{t(`pages.aiAssistant.contextMeter.request.${key}`)}</dt>
                        <dd className="text-right tabular-nums">{budget ? <>{bytes(budget[key])}<span className="block text-xs text-muted-foreground">{share(budget[key], budget.totalBytes)}</span></> : '—'}</dd>
                    </div>)}
                </dl>
                {!budget && <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.contextMeter.requestUnknown')}</p>}
            </section>
            <p className="border-t pt-2 text-sm font-medium">{t('pages.aiAssistant.contextMeter.historyTitle')}</p>
            <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1 text-sm [&_dd]:text-right [&_dd]:tabular-nums">
                <dt>{t(`pages.aiAssistant.contextMeter.limit.${usage.strategy}`)}</dt><dd>{bytes(usage.limitBytes)}</dd>
                <dt>{t('pages.aiAssistant.contextMeter.used')}</dt><dd>{bytes(usage.usedBytes)}</dd>
                {usage.breakdown && (['messagesBytes', 'toolsBytes', 'replayBytes', 'projectedBytes'] as const).map(key => <div key={key} className="contents">
                    <dt className="pl-2 text-xs text-muted-foreground">{t(`pages.aiAssistant.contextMeter.breakdown.${key}`)}</dt><dd className="text-xs">{bytes(usage.breakdown[key])}</dd>
                </div>)}
                <dt>{t('pages.aiAssistant.contextMeter.remaining')}</dt><dd>{bytes(values.remaining)}</dd>
                <dt>{t('pages.aiAssistant.contextMeter.draft')}</dt><dd>{bytes(values.draftBytes)}</dd>
            </dl>
            {values.draftBytes > values.remaining && <p>{t('pages.aiAssistant.contextMeter.exceeds')}</p>}
            <p className="text-xs leading-relaxed text-muted-foreground">{t('pages.aiAssistant.contextMeter.hint')}</p>
        </> : <p>{t('pages.aiAssistant.contextMeter.unknownHint')}</p>}
    </PopoverContent></Popover>;
}
