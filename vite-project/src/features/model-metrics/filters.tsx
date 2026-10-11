import { useTranslation } from 'react-i18next';
import type { GetModelMetricsOverviewQueryParams, MetricsGroups } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Disclosure } from '@/components/ui/disclosure';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { SelectItem } from '@/components/ui/select';
import { SelectField } from '@/components/ui/select-field';
import { X } from 'lucide-react';
import { ERROR_SELECTORS } from './format';

export type MetricsQuery = GetModelMetricsOverviewQueryParams;

const CLOSED_FILTERS = {
    surface: ['assistant', 'diagnose', 'terminal', 'fleet', 'support', 'safety', 'probe', 'platform'],
    purpose: ['agent', 'approval', 'completion', 'context_compression', 'safety', 'probe', 'fleet_natural_language', 'support'],
    origin: ['user', 'permission_resume', 'work_completion', 'subagent', 'goal_continuation', 'scheduled_task', 'system', 'unknown'],
} as const;

export function MetricsActiveFilters({ value, change }: {
    value: MetricsQuery;
    change: (value: MetricsQuery) => void;
}) {
    const { t } = useTranslation();
    const fields = ['provider_id', 'model_id', 'surface', 'purpose', 'origin', 'tool', 'error', 'contract_revision'] as const;
    const active = fields.flatMap((field) => {
        const selected = value[field];
        if (!selected) return [];
        const text = field === 'error' ? t(`modelMetrics.error.${selected}`)
            : field in CLOSED_FILTERS ? t(`modelMetrics.value.${field}.${selected}`) : selected;
        return [{ field, text, label: t(`modelMetrics.filter.${field}`) }];
    });
    if (active.length === 0 && !value.include_probe) return null;
    return <div role="group" aria-label={t('modelMetrics.activeFilters')} className="flex flex-wrap items-center gap-2 rounded border bg-muted/30 p-3">
        <span className="text-sm font-medium">{t('modelMetrics.activeFilters')}</span>
        {active.map(({ field, text, label }) => <Button key={field} variant="secondary" size="sm" className="h-auto max-w-full gap-2 py-1.5" aria-label={t('modelMetrics.removeFilter', { filter: `${label}: ${text}` })} onClick={() => change({ ...value, [field]: undefined })}>
            <span className="whitespace-normal break-all text-left">{label}: {text}</span><X className="size-3 shrink-0" aria-hidden="true"/>
        </Button>)}
        {value.include_probe && <Button variant="secondary" size="sm" className="h-auto max-w-full gap-2 py-1.5" aria-label={t('modelMetrics.removeFilter', { filter: t('modelMetrics.includeProbe') })} onClick={() => change({ ...value, include_probe: undefined })}>
            <span className="whitespace-normal text-left">{t('modelMetrics.includeProbe')}</span><X className="size-3 shrink-0" aria-hidden="true"/>
        </Button>}
        <Button variant="ghost" size="sm" onClick={() => change({ granularity: value.granularity, limit: value.limit })}>{t('modelMetrics.clearFilters')}</Button>
    </div>;
}

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
            <Label className="grid min-w-0 gap-2">{t('modelMetrics.model')}
                <SelectField aria-label={t('modelMetrics.model')} value={modelValue} onValueChange={(next) => {
                    const selected = modelOptions.find((group) => JSON.stringify([group.provider_id ?? '', group.model_id]) === next);
                    change({ ...value, model_id: selected?.model_id ?? undefined, provider_id: selected?.provider_id ?? undefined });
                }}>
                    <SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>
                    {unknownModel && <SelectItem value={modelValue}>{t('modelMetrics.exactModelSelection')}</SelectItem>}
                    {modelOptions.map((group) => <SelectItem key={group.key} value={JSON.stringify([group.provider_id ?? '', group.model_id])}>{group.model_name} · {group.provider_id} / {group.model_id}</SelectItem>)}
                </SelectField>
            </Label>
            {Object.entries(CLOSED_FILTERS).map(([name, choices]) => <Label key={name} className="grid min-w-0 gap-2">{t(`modelMetrics.filter.${name}`)}
                <SelectField aria-label={t(`modelMetrics.filter.${name}`)} value={value[name as keyof typeof CLOSED_FILTERS] ?? ''} onValueChange={(next) => change({ ...value, [name]: next || undefined })}>
                    <SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>
                    {choices.map((choice) => <SelectItem key={choice} value={choice}>{t(`modelMetrics.value.${name}.${choice}`)}</SelectItem>)}
                </SelectField>
            </Label>)}
            <Label className="grid min-w-0 gap-2">{t('modelMetrics.filter.tool')}
                <SelectField aria-label={t('modelMetrics.filter.tool')} value={value.tool ? JSON.stringify(value.tool) : ''} onValueChange={(next) => change({ ...value, tool: next ? JSON.parse(next) as string : undefined })}>
                    <SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>
                    {value.tool && !toolOptions.includes(value.tool) && <SelectItem value={JSON.stringify(value.tool)}>{value.tool}</SelectItem>}
                    {toolOptions.map((tool) => <SelectItem key={tool} value={JSON.stringify(tool)}>{tool}</SelectItem>)}
                </SelectField>
            </Label>
            <Label className="grid min-w-0 gap-2">{t('modelMetrics.filter.error')}
                <SelectField aria-label={t('modelMetrics.filter.error')} value={value.error ?? ''} onValueChange={(next) => change({ ...value, error: next || undefined })}>
                    <SelectItem value="__empty__">{t('modelMetrics.all')}</SelectItem>
                    {ERROR_SELECTORS.map((error) => <SelectItem key={error} value={error}>{t(`modelMetrics.error.${error}`)}</SelectItem>)}
                </SelectField>
            </Label>
            <Label className="grid min-w-0 gap-2">{t('modelMetrics.granularity')}
                <SelectField aria-label={t('modelMetrics.granularity')} value={value.granularity ?? 'hour'} onValueChange={(next) => change({ ...value, granularity: next as MetricsQuery['granularity'] })}>
                    <SelectItem value="hour">{t('modelMetrics.hour')}</SelectItem><SelectItem value="five_minutes">{t('modelMetrics.fiveMinutes')}</SelectItem>
                </SelectField>
            </Label>
            <Label className="flex gap-2 items-center"><Checkbox aria-label={t('modelMetrics.includeProbe')} checked={!!value.include_probe} onCheckedChange={(checked) => change({ ...value, include_probe: checked === true })}/>{t('modelMetrics.includeProbe')}</Label>
        </div>
        {loading && <p className="text-sm text-muted-foreground">{t('modelMetrics.loadingChoices')}</p>}
        {failed && <p className="text-sm text-muted-foreground">{t('modelMetrics.choicesUnavailable')}</p>}
        {(models?.other || tools?.other || models?.coverage.sample_status === 'partial' || tools?.coverage.sample_status === 'partial') && <p className="text-sm text-muted-foreground">{t('modelMetrics.choicesBounded')}</p>}
        <Disclosure summaryClassName="text-sm" title={t('modelMetrics.exactFilters')}>
            <div className="grid gap-3 pt-3 sm:grid-cols-2 lg:grid-cols-4">{(['provider_id', 'model_id', 'tool', 'contract_revision'] as const).map((field) => <Label key={field} className="space-y-2">{t(`modelMetrics.filter.${field}`)}<Input aria-label={t(`modelMetrics.filter.${field}`)} value={String(value[field] ?? '')} maxLength={field === 'contract_revision' ? 64 : 128} onChange={(event) => change({ ...value, [field]: event.target.value || undefined })}/></Label>)}</div>
            <p className="text-sm text-muted-foreground pt-2">{t('modelMetrics.exactFilterHint')}</p>
        </Disclosure>
        <Button variant="outline" size="sm" onClick={() => change({ granularity: value.granularity, limit: value.limit })}>{t('modelMetrics.clearFilters')}</Button>
    </div>;
}
