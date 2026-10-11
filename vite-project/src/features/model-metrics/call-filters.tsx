import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { SelectItem } from '@/components/ui/select';
import { SelectField } from '@/components/ui/select-field';
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
            <Label className="grid gap-2">{t('modelMetrics.field.kind')}<SelectField aria-label={t('modelMetrics.field.kind')} className="w-52 max-w-full" value={value.record_kind ?? ''} onValueChange={(next) => change({ record_kind: next as CallConditions['record_kind'] || undefined })}>
                <SelectItem value="__empty__">{t('modelMetrics.automaticKind')}</SelectItem>{['call', 'attempt', 'tool', 'operation', 'runtime'].map((type) => <SelectItem key={type} value={type}>{t(`modelMetrics.value.kind.${type}`)}</SelectItem>)}
            </SelectField></Label>
            <Label className="grid gap-2">{t('modelMetrics.outcome')}<SelectField aria-label={t('modelMetrics.outcome')} className="w-52 max-w-full" value={value.outcome ?? ''} onValueChange={(next) => change({ ...value, outcome: next || undefined })} disabled={kind === 'runtime'}>
                <SelectItem value="__empty__">{t('modelMetrics.anyOutcome')}</SelectItem>{outcomes.map((outcome) => <SelectItem key={outcome} value={outcome}>{t(outcome === 'request_error' ? 'modelMetrics.count.request_errors' : `modelMetrics.value.${outcomeGroup}.${outcome}`)}</SelectItem>)}
            </SelectField></Label>
            <Label className="space-y-2">{t('modelMetrics.minimumDuration')}<Input className="w-36" type="number" min="0" max="86400000" step="1" aria-label={t('modelMetrics.minimumDuration')} value={value.min_duration_ms ?? ''} onChange={(event) => {
                const minimum = event.target.value === '' ? undefined : Number(event.target.value);
                if (minimum == null || (Number.isInteger(minimum) && minimum >= 0 && minimum <= 86400000)) change({ ...value, min_duration_ms: minimum });
            }}/></Label>
            <Label className="grid gap-2">{t('modelMetrics.latencyBasis')}<SelectField aria-label={t('modelMetrics.latencyBasis')} className="w-52 max-w-full" value={value.latency ?? 'duration'} onValueChange={(next) => change({ ...value, latency: next as CallConditions['latency'] })}>
                <SelectItem value="duration">{t('modelMetrics.duration')}</SelectItem>{['call', 'attempt'].includes(kind) && <SelectItem value="first_content">{t('modelMetrics.firstContent')}</SelectItem>}
            </SelectField></Label>
            <Button variant="outline" disabled={kind === 'runtime'} onClick={() => change({ ...value, outcome: errorOutcome })}>{t('modelMetrics.errorsOnly')}</Button>
            <Button variant="outline" onClick={() => change({ ...value, min_duration_ms: 5000 })}>{t('modelMetrics.slowOnly')}</Button>
            <Button variant="ghost" onClick={() => change({})}>{t('modelMetrics.clearCallFilters')}</Button>
        </div>
        {(value.permission != null || value.dispatched != null) && <p className="text-sm">{value.permission != null ? `${t('modelMetrics.field.permission')}: ${t(`modelMetrics.value.permission.${value.permission}`)}` : `${t('modelMetrics.field.dispatched')}: ${t(value.dispatched ? 'modelMetrics.value.yes' : 'modelMetrics.value.no')}`}</p>}
    </div>;
}
