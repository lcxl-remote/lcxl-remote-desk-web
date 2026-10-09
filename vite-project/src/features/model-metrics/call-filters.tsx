import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import type { CallConditions } from './drill-down';

export function CallFilters({ value, change, inferredKind }: { value: CallConditions; change: (value: CallConditions) => void; inferredKind: string }) {
    const { t } = useTranslation();
    const kind = value.record_kind ?? inferredKind;
    const outcomeGroup = kind === 'tool' ? 'input' : kind === 'operation' ? 'operation' : 'request';
    const outcomes = kind === 'runtime' ? [] : kind === 'tool' ? ['accepted', 'rejected', 'unknown'] : kind === 'operation' ? ['pending', 'verified', 'accepted', 'changed_unverified', 'failed', 'unknown', 'cancelled', 'rejected'] : ['request_error', 'not_started', 'pending', 'returned', 'http_error', 'provider_error', 'transport_error', 'timeout', 'stream_error', 'cancelled', 'observation_incomplete'];
    const errorOutcome = kind === 'tool' ? 'rejected' : kind === 'operation' ? 'failed' : 'request_error';
    return <div className="space-y-3 rounded border p-3">
        <p className="text-sm text-muted-foreground">{t('modelMetrics.callFiltersHint')}</p>
        <div className="flex flex-wrap items-end gap-3">
            <Label className="space-y-2">{t('modelMetrics.field.kind')}<select aria-label={t('modelMetrics.field.kind')} className="block h-9 rounded border bg-background px-2" value={value.record_kind ?? ''} onChange={(event) => change({ record_kind: event.target.value as CallConditions['record_kind'] || undefined })}>
                <option value="">{t('modelMetrics.automaticKind')}</option>{['call', 'attempt', 'tool', 'operation', 'runtime'].map((type) => <option key={type} value={type}>{t(`modelMetrics.value.kind.${type}`)}</option>)}
            </select></Label>
            <Label className="space-y-2">{t('modelMetrics.outcome')}<select aria-label={t('modelMetrics.outcome')} className="block h-9 rounded border bg-background px-2" value={value.outcome ?? ''} onChange={(event) => change({ ...value, outcome: event.target.value || undefined })} disabled={kind === 'runtime'}>
                <option value="">{t('modelMetrics.anyOutcome')}</option>{outcomes.map((outcome) => <option key={outcome} value={outcome}>{t(outcome === 'request_error' ? 'modelMetrics.count.request_errors' : `modelMetrics.value.${outcomeGroup}.${outcome}`)}</option>)}
            </select></Label>
            <Label className="space-y-2">{t('modelMetrics.minimumDuration')}<Input className="w-36" type="number" min="0" max="86400000" step="1" aria-label={t('modelMetrics.minimumDuration')} value={value.min_duration_ms ?? ''} onChange={(event) => {
                const minimum = event.target.value === '' ? undefined : Number(event.target.value);
                if (minimum == null || (Number.isInteger(minimum) && minimum >= 0 && minimum <= 86400000)) change({ ...value, min_duration_ms: minimum });
            }}/></Label>
            <Label className="space-y-2">{t('modelMetrics.latencyBasis')}<select aria-label={t('modelMetrics.latencyBasis')} className="block h-9 rounded border bg-background px-2" value={value.latency ?? 'duration'} onChange={(event) => change({ ...value, latency: event.target.value as CallConditions['latency'] })}>
                <option value="duration">{t('modelMetrics.duration')}</option>{['call', 'attempt'].includes(kind) && <option value="first_content">{t('modelMetrics.firstContent')}</option>}
            </select></Label>
            <Button variant="outline" disabled={kind === 'runtime'} onClick={() => change({ ...value, outcome: errorOutcome })}>{t('modelMetrics.errorsOnly')}</Button>
            <Button variant="outline" onClick={() => change({ ...value, min_duration_ms: 5000 })}>{t('modelMetrics.slowOnly')}</Button>
            <Button variant="ghost" onClick={() => change({})}>{t('modelMetrics.clearCallFilters')}</Button>
        </div>
        {(value.permission != null || value.dispatched != null) && <p className="text-sm">{value.permission != null ? `${t('modelMetrics.field.permission')}: ${t(`modelMetrics.value.permission.${value.permission}`)}` : `${t('modelMetrics.field.dispatched')}: ${t(value.dispatched ? 'modelMetrics.value.yes' : 'modelMetrics.value.no')}`}</p>}
    </div>;
}
