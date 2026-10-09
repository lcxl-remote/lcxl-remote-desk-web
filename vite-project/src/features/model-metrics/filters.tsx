import { useTranslation } from 'react-i18next';
import type { GetModelMetricsOverviewQueryParams, MetricsGroups } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { ERROR_SELECTORS } from './format';

export type MetricsQuery = GetModelMetricsOverviewQueryParams;

const CLOSED_FILTERS = {
    surface: ['assistant', 'diagnose', 'terminal', 'fleet', 'support', 'safety', 'probe', 'platform'],
    purpose: ['agent', 'approval', 'completion', 'context_compression', 'safety', 'probe', 'fleet_natural_language', 'support'],
    origin: ['user', 'permission_resume', 'work_completion', 'subagent', 'goal_continuation', 'scheduled_task', 'system', 'unknown'],
} as const;

export function MetricsFilters({ value, change, models, tools, loading, failed }: {
    value: MetricsQuery;
    change: (value: MetricsQuery) => void;
    models?: MetricsGroups;
    tools?: MetricsGroups;
    loading: boolean;
    failed: boolean;
}) {
    const { t } = useTranslation();
    const modelValue = value.model_id ? JSON.stringify([value.provider_id ?? '', value.model_id]) : '';
    const modelOptions = models?.groups.filter((group) => group.model_id != null) ?? [];
    const toolOptions = tools?.groups.flatMap((group) => group.tool ? [group.tool] : []) ?? [];
    const unknownModel = modelValue && !modelOptions.some((group) => JSON.stringify([group.provider_id ?? '', group.model_id]) === modelValue);
    return <div className="space-y-3 pt-3">
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Label className="space-y-2">{t('modelMetrics.model')}
                <select aria-label={t('modelMetrics.model')} className="w-full h-9 border rounded bg-background" value={modelValue} onChange={(event) => {
                    const selected = modelOptions.find((group) => JSON.stringify([group.provider_id ?? '', group.model_id]) === event.target.value);
                    change({ ...value, model_id: selected?.model_id ?? undefined, provider_id: selected?.provider_id ?? undefined });
                }}>
                    <option value="">{t('modelMetrics.all')}</option>
                    {unknownModel && <option value={modelValue}>{t('modelMetrics.exactModelSelection')}</option>}
                    {modelOptions.map((group) => <option key={group.key} value={JSON.stringify([group.provider_id ?? '', group.model_id])}>{group.model_name} · {group.provider_id} / {group.model_id}</option>)}
                </select>
            </Label>
            {Object.entries(CLOSED_FILTERS).map(([name, choices]) => <Label key={name} className="space-y-2">{t(`modelMetrics.filter.${name}`)}
                <select aria-label={t(`modelMetrics.filter.${name}`)} className="w-full h-9 border rounded bg-background" value={value[name as keyof typeof CLOSED_FILTERS] ?? ''} onChange={(event) => change({ ...value, [name]: event.target.value || undefined })}>
                    <option value="">{t('modelMetrics.all')}</option>
                    {choices.map((choice) => <option key={choice} value={choice}>{t(`modelMetrics.value.${name}.${choice}`)}</option>)}
                </select>
            </Label>)}
            <Label className="space-y-2">{t('modelMetrics.filter.tool')}
                <select aria-label={t('modelMetrics.filter.tool')} className="w-full h-9 border rounded bg-background" value={value.tool ?? ''} onChange={(event) => change({ ...value, tool: event.target.value || undefined })}>
                    <option value="">{t('modelMetrics.all')}</option>
                    {value.tool && !toolOptions.includes(value.tool) && <option value={value.tool}>{value.tool}</option>}
                    {toolOptions.map((tool) => <option key={tool} value={tool}>{tool}</option>)}
                </select>
            </Label>
            <Label className="space-y-2">{t('modelMetrics.filter.error')}
                <select aria-label={t('modelMetrics.filter.error')} className="w-full h-9 border rounded bg-background" value={value.error ?? ''} onChange={(event) => change({ ...value, error: event.target.value || undefined })}>
                    <option value="">{t('modelMetrics.all')}</option>
                    {ERROR_SELECTORS.map((error) => <option key={error} value={error}>{t(`modelMetrics.error.${error}`)}</option>)}
                </select>
            </Label>
            <Label className="space-y-2">{t('modelMetrics.granularity')}
                <select aria-label={t('modelMetrics.granularity')} className="w-full h-9 border rounded bg-background" value={value.granularity ?? 'hour'} onChange={(event) => change({ ...value, granularity: event.target.value as MetricsQuery['granularity'] })}>
                    <option value="hour">{t('modelMetrics.hour')}</option><option value="five_minutes">{t('modelMetrics.fiveMinutes')}</option>
                </select>
            </Label>
            <Label className="flex gap-2 items-center"><input type="checkbox" checked={!!value.include_probe} onChange={(event) => change({ ...value, include_probe: event.target.checked })}/>{t('modelMetrics.includeProbe')}</Label>
        </div>
        {loading && <p className="text-sm text-muted-foreground">{t('modelMetrics.loadingChoices')}</p>}
        {failed && <p className="text-sm text-muted-foreground">{t('modelMetrics.choicesUnavailable')}</p>}
        {(models?.other || tools?.other || models?.coverage.sample_status === 'partial' || tools?.coverage.sample_status === 'partial') && <p className="text-sm text-muted-foreground">{t('modelMetrics.choicesBounded')}</p>}
        <details><summary className="text-sm">{t('modelMetrics.exactFilters')}</summary>
            <div className="grid gap-3 pt-3 sm:grid-cols-2 lg:grid-cols-4">{(['provider_id', 'model_id', 'tool', 'contract_revision'] as const).map((field) => <Label key={field} className="space-y-2">{t(`modelMetrics.filter.${field}`)}<Input aria-label={t(`modelMetrics.filter.${field}`)} value={String(value[field] ?? '')} maxLength={field === 'contract_revision' ? 64 : 128} onChange={(event) => change({ ...value, [field]: event.target.value || undefined })}/></Label>)}</div>
            <p className="text-sm text-muted-foreground pt-2">{t('modelMetrics.exactFilterHint')}</p>
        </details>
        <Button variant="outline" size="sm" onClick={() => change({ granularity: value.granularity, limit: value.limit })}>{t('modelMetrics.clearFilters')}</Button>
    </div>;
}
