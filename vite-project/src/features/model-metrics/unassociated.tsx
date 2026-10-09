import { useTranslation } from 'react-i18next';
import type { MetricsUnassociated } from '@/services/types';
import { Button } from '@/components/ui/button';
import { MetricsCoverage } from './coverage';
import { MetricTime } from './time';

export function UnassociatedFacts({ value, select }: { value: MetricsUnassociated; select: (id: string) => void }) {
    const { t } = useTranslation();
    return <div className="space-y-4">
        <p className="text-sm text-muted-foreground">{t('modelMetrics.unassociated.hint')}</p>
        <MetricsCoverage value={value.coverage} empty={value.records.length === 0}/>
        <div className="overflow-x-auto"><table className="w-full text-sm"><thead><tr className="border-b text-left">
            <th>{t('modelMetrics.unassociated.received')}</th><th>{t('modelMetrics.field.kind')}</th>
            <th>{t('modelMetrics.unassociated.missing')}</th><th>{t('modelMetrics.state')}</th><th>{t('modelMetrics.model')}</th><th>{t('modelMetrics.detail')}</th>
        </tr></thead><tbody>{value.records.map((row) => <tr key={row.id} className="border-b align-top">
            <td className="py-3"><MetricTime value={row.received_at}/></td><td>{t(`modelMetrics.value.kind.${row.kind}`)}</td>
            <td>{t(`modelMetrics.unassociated.missing.${row.missing}`)}</td><td>{t(`modelMetrics.unassociated.state.${row.state}`)}</td>
            <td>{row.original_model?.model_name ?? t('modelMetrics.value.unknown')}<p className="text-xs text-muted-foreground">{row.tool ?? ''}</p></td>
            <td className="py-3"><details><summary>{t('modelMetrics.detail')}</summary><dl className="space-y-2 pt-2 min-w-64">
                <div><dt>{t('modelMetrics.field.id')}</dt><dd className="font-mono break-all">{row.id}</dd></div>
                <div><dt>{t('modelMetrics.field.started_at')}</dt><dd><MetricTime value={row.started_at}/></dd></div>
                <div><dt>{t('modelMetrics.unassociated.occurred')}</dt><dd><MetricTime value={row.occurred_at}/></dd></div>
                <div><dt>{t('modelMetrics.field.updated_at')}</dt><dd><MetricTime value={row.updated_at}/></dd></div>
                <div><dt>{t('modelMetrics.unassociated.phase')}</dt><dd>{t(`modelMetrics.unassociated.phase.${row.phase}`)}</dd></div>
                {row.ordinal != null && <div><dt>{t('modelMetrics.field.ordinal')}</dt><dd>{row.ordinal}</dd></div>}
                {row.call_id && <div><dt>{t('modelMetrics.field.call_id')}</dt><dd className="font-mono break-all">{row.call_id}</dd></div>}
                {row.permission && <div><dt>{t('modelMetrics.field.permission')}</dt><dd>{t(`modelMetrics.value.permission.${row.permission}`)}</dd></div>}
                {row.fact_outcome && <div><dt>{t('modelMetrics.outcome')}</dt><dd>{t(`modelMetrics.value.operation.${row.fact_outcome}`)}</dd></div>}
                {row.original_model && <>
                    <div><dt>{t('modelMetrics.field.provider_id')}</dt><dd className="font-mono break-all">{row.original_model.provider_id}</dd></div>
                    <div><dt>{t('modelMetrics.field.model_id')}</dt><dd className="font-mono break-all">{row.original_model.model_id}</dd></div>
                    <div><dt>{t('modelMetrics.field.configuration_revision')}</dt><dd>{row.original_model.configuration_revision}</dd></div>
                    <div><dt>{t('modelMetrics.field.contract_revision')}</dt><dd>{row.original_model.contract_revision}</dd></div>
                    <div><dt>{t('modelMetrics.field.purpose')}</dt><dd>{t(`modelMetrics.value.purpose.${row.original_model.purpose}`)}</dd></div>
                    <div><dt>{t('modelMetrics.field.surface')}</dt><dd>{t(`modelMetrics.value.surface.${row.original_model.surface}`)}</dd></div>
                    <div><dt>{t('modelMetrics.field.origin')}</dt><dd>{t(`modelMetrics.value.origin.${row.original_model.origin}`)}</dd></div>
                    <div><dt>{t('modelMetrics.field.configuration_scope')}</dt><dd>{t(`modelMetrics.value.scope.${row.original_model.configuration_scope}`)}</dd></div>
                    <div><dt>{t('modelMetrics.field.protocol')}</dt><dd>{t(`modelMetrics.value.protocol.${row.original_model.protocol}`)}</dd></div>
                </>}
                {row.tool_observation_id && <Button variant="outline" size="sm" onClick={() => select(row.tool_observation_id!)}>{t('modelMetrics.unassociated.originalTool')}</Button>}
                {!row.tool_observation_id && row.call_id && row.original_model && <Button variant="outline" size="sm" onClick={() => select(row.call_id!)}>{t('modelMetrics.unassociated.originalCall')}</Button>}
            </dl></details></td>
        </tr>)}</tbody></table></div>
    </div>;
}
