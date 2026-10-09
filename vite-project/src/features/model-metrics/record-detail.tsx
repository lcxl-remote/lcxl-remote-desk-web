import { useTranslation } from 'react-i18next';
import type { ObservationRecord } from '@/services/types';
import { Button } from '@/components/ui/button';
import { observationText, metricTime } from './format';
import { MetricTime } from './time';

const enums: Partial<Record<keyof ObservationRecord, string>> = {
    kind: 'kind', purpose: 'purpose', surface: 'surface', origin: 'origin',
    tool_counts_status: 'state', configuration_scope: 'scope', protocol: 'protocol', output: 'output',
    input_conclusion: 'input', input_issue: 'issue', permission: 'permission',
    correction_status: 'correction', correction_input: 'input',
    not_started_reason: 'notStartedReason',
};

function outcomeGroup(record: ObservationRecord) {
    switch (record.kind) {
        case 'call': case 'attempt': return 'request';
        case 'tool': return 'input';
        case 'operation': return 'operation';
        default: return undefined;
    }
}

export function RecordOutcome({ record }: { record: ObservationRecord }) {
    const { t } = useTranslation();
    const group = outcomeGroup(record);
    return <>{group ? t(`modelMetrics.value.${group}.${record.outcome}`) : t(`modelMetrics.runtimeDefinition.${record.outcome}`)}</>;
}

export function ObservationDetails({ record, expanded, select }: { record: ObservationRecord; expanded: boolean; select: (id: string) => void }) {
    const { t, i18n } = useTranslation();
    const fields = Object.entries(record).filter(([field, value]) => !['stages', 'correction_group', 'correction_group_unavailable'].includes(field) && (value != null || (record.kind === 'operation' && field === 'dispatched')));
    return <details open={expanded} className="border rounded p-3 space-y-3">
        <summary>{t(`modelMetrics.value.kind.${record.kind}`)} · {record.tool || record.model_name || t('modelMetrics.value.unknown')} · <RecordOutcome record={record}/></summary>
        <dl className="grid grid-cols-1 sm:grid-cols-2 gap-3 text-sm">
            {fields.map(([field, value]) => {
                const parent = field === 'call_id' || field === 'tool_observation_id' || field === 'correction_of' || field === 'correction_group_root';
                const group = field === 'outcome' ? outcomeGroup(record) : enums[field as keyof ObservationRecord];
                let shown = value == null || value === '' ? t('modelMetrics.value.unknown')
                    : typeof value === 'boolean' ? t(value ? 'modelMetrics.value.yes' : 'modelMetrics.value.no')
                    : group ? t(group === 'state' ? `modelMetrics.state.${String(value)}` : `modelMetrics.value.${group}.${String(value)}`)
                    : field === 'started_at' || field === 'updated_at' ? metricTime(String(value), i18n.language)
                    : field.endsWith('_ms') ? t('modelMetrics.milliseconds', { value })
                    : field === 'ordinal' && (record.kind === 'operation' || record.kind === 'tool') ? String(Number(value) + 1)
                    : String(value);
                if ((parent || field === 'id') && typeof value === 'string') shown = observationText(value);
                if (record.kind === 'runtime' && field === 'outcome') shown = t(`modelMetrics.runtimeDefinition.${record.outcome}`);
                return <div key={field}>
                    <dt className="text-muted-foreground">{t(`modelMetrics.field.${field}`)}</dt>
                    <dd className="break-all">{parent && typeof value === 'string' && value !== record.id
                        ? <Button variant="link" className="h-auto p-0 text-left whitespace-normal" onClick={() => select(value)}>{shown}</Button>
                        : shown}</dd>
                </div>;
            })}
        </dl>
        {record.correction_group && <section className="border rounded p-3 space-y-2" aria-label={t('modelMetrics.correctionGroup')}>
            <p className="font-medium">{t('modelMetrics.correctionGroup')}</p>
            <dl className="grid grid-cols-1 sm:grid-cols-2 gap-3 text-sm">
                <div><dt className="text-muted-foreground">{t('modelMetrics.correctionGroupCategory')}</dt><dd>{t(`modelMetrics.value.correctionCategory.${record.correction_group.category}`)}</dd></div>
                <div><dt className="text-muted-foreground">{t('modelMetrics.correctionGroupOutcome')}</dt><dd>{t(`modelMetrics.value.correctionGroup.${record.correction_group.outcome}`)}</dd></div>
                <div><dt className="text-muted-foreground">{t(record.kind === 'call' ? 'modelMetrics.correctionGroupRequests' : 'modelMetrics.correctionGroupAttempts')}</dt><dd>{record.correction_group.linked_attempts}</dd></div>
                {record.correction_group.reason && <div><dt className="text-muted-foreground">{t('modelMetrics.correctionGroupReason')}</dt><dd>{t(`modelMetrics.value.protocolCorrectionReason.${record.correction_group.reason}`)}</dd></div>}
                <div><dt className="text-muted-foreground">{t('modelMetrics.correctionGroupUpdated')}</dt><dd><MetricTime value={record.correction_group.updated_at}/></dd></div>
                <div><dt className="text-muted-foreground">{t(record.kind === 'call' ? 'modelMetrics.correctionGroupLastRequest' : 'modelMetrics.correctionGroupLastInput')}</dt><dd className="break-all">{record.correction_group.last_input_id !== record.id
                    ? <Button variant="link" className="h-auto p-0 text-left whitespace-normal" onClick={() => select(record.correction_group!.last_input_id)}>{observationText(record.correction_group.last_input_id)}</Button>
                    : observationText(record.correction_group.last_input_id)}</dd></div>
            </dl>
            <p className="text-sm text-muted-foreground">{t(record.kind === 'call' ? 'modelMetrics.protocolCorrectionGroupHint' : 'modelMetrics.correctionGroupHint')}</p>
        </section>}
        {record.correction_group_unavailable && <p className="text-sm text-muted-foreground">{t('modelMetrics.correctionGroupUnavailable')}</p>}
        {record.kind === 'operation' && <p className="text-sm text-muted-foreground">{t('modelMetrics.operationCertaintyHint')}</p>}
        {record.stages.length > 0 && <div className="overflow-x-auto"><table className="w-full text-sm">
            <thead><tr className="text-left border-b"><th>{t('modelMetrics.field.stages')}</th><th>{t('modelMetrics.outcome')}</th></tr></thead>
            <tbody>{record.stages.map((stage) => <tr key={stage.stage} className="border-b">
                <td className="py-2">{t(`modelMetrics.value.stage.${stage.stage}`)}</td><td>{t(`modelMetrics.value.stageOutcome.${stage.outcome}`)}</td>
            </tr>)}</tbody>
        </table></div>}
    </details>;
}
