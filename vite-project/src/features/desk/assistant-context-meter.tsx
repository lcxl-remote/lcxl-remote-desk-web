import { useTranslation } from 'react-i18next';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import type { ContextUsageDto } from '@/services/types';

export type AssistantContextUsage = ContextUsageDto;

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
    const bytes = (n: number) => t('pages.deviceAssistant.contextMeter.bytes', { value: new Intl.NumberFormat(i18n.language).format(n) });
    const label = t(values ? 'pages.deviceAssistant.contextMeter.percent' : 'pages.deviceAssistant.contextMeter.unknown', { percent: values?.percent });
    return <TooltipProvider><Tooltip><TooltipTrigger asChild>
        <button type="button" aria-label={label} className="flex h-11 w-11 shrink-0 items-center justify-center rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
            <svg viewBox="0 0 40 40" className="h-10 w-10" aria-hidden="true">
                <circle cx="20" cy="20" r="17" fill="none" stroke="currentColor" strokeWidth="3" className="text-muted" />
                {values && <circle cx="20" cy="20" r="17" fill="none" stroke="currentColor" strokeWidth="3"
                    pathLength="100" strokeDasharray={`${values.percent} 100`} transform="rotate(-90 20 20)"
                    className={values.percent >= 90 ? 'text-amber-500' : 'text-primary'} />}
                <text x="20" y="20" dy=".35em" textAnchor="middle" fill="currentColor" fontSize="10">{values ? `${values.percent}%` : '—'}</text>
            </svg>
        </button>
    </TooltipTrigger><TooltipContent side="top" className="max-w-xs space-y-2 p-3">
        <p className="font-medium">{t('pages.deviceAssistant.contextMeter.title')}</p>
        {values && usage ? <>
            <dl className="grid grid-cols-[1fr_auto] gap-x-4 gap-y-1">
                <dt>{t(`pages.deviceAssistant.contextMeter.limit.${usage.strategy}`)}</dt><dd>{bytes(usage.limitBytes)}</dd>
                <dt>{t('pages.deviceAssistant.contextMeter.used')}</dt><dd>{bytes(usage.usedBytes)}</dd>
                <dt>{t('pages.deviceAssistant.contextMeter.remaining')}</dt><dd>{bytes(values.remaining)}</dd>
                <dt>{t('pages.deviceAssistant.contextMeter.draft')}</dt><dd>{bytes(values.draftBytes)}</dd>
            </dl>
            {values.draftBytes > values.remaining && <p>{t('pages.deviceAssistant.contextMeter.exceeds')}</p>}
            <p>{t('pages.deviceAssistant.contextMeter.hint')}</p>
        </> : <p>{t('pages.deviceAssistant.contextMeter.unknownHint')}</p>}
    </TooltipContent></Tooltip></TooltipProvider>;
}
