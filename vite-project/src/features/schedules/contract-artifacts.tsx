import { useTranslation } from 'react-i18next';
import type { Dispatch, SetStateAction } from 'react';
import type { ScheduleManagementResponse } from '@/services/types';

type Contract = NonNullable<Extract<ScheduleManagementResponse, { result: 'task_contract' }>['contract']>;

export function ContractArtifacts({ contract, original, setDraft }: {
    contract: Contract; original?: Contract; setDraft?: Dispatch<SetStateAction<Contract>>;
}) {
    const { t } = useTranslation();
    return <>{contract.steps.map(step => {
        if (step.binding.kind !== 'produce_text_artifact') return null;
        const binding = step.binding;
        const rule = contract.permissions.find(item => item.rule_id === step.rule_id);
        if (rule?.input.kind !== 'generated_text_artifact') return null;
        const input = rule.input;
        const previous = original?.steps.find(item => item.step_id === step.step_id)?.binding;
        return <section className="space-y-2 rounded border p-3" key={step.step_id}>
            <h3 className="font-semibold">{t('schedules.artifact.title')}</h3>
            <p className="break-all">{t('schedules.artifact.directory')}: {binding.canonical_directory}</p>
            <p className="break-all">{t('schedules.artifact.fileName')}: {input.file_name}</p>
            {setDraft ? <>
                <p className="text-sm">{t('schedules.artifact.fixed')}</p>
                <label className="block">{t('schedules.artifact.maxBytes')}
                    <input className="ml-2 rounded border bg-background p-2" type="number" min={1} max={65536} step={1} required
                        value={input.max_content_bytes || ''} onChange={event => {
                            const value = Number(event.target.value);
                            setDraft(current => ({ ...current, permissions: current.permissions.map(item =>
                                item.rule_id === rule.rule_id && item.input.kind === 'generated_text_artifact'
                                    ? { ...item, input: { ...item.input, max_content_bytes: Number.isSafeInteger(value) ? value : 0 } } : item) }));
                        }} />
                </label>
                <p>{t('schedules.contract.sources')}</p>
                {previous?.kind === 'produce_text_artifact' && previous.allowed_source_scopes.map(source => <label className="flex items-center gap-2 break-all" key={source}>
                    <input type="checkbox" checked={binding.allowed_source_scopes.includes(source)} onChange={event => {
                        const checked = event.target.checked;
                        setDraft(current => ({ ...current, steps: current.steps.map(item =>
                            item.step_id === step.step_id && item.binding.kind === 'produce_text_artifact'
                                ? { ...item, binding: { ...item.binding, allowed_source_scopes: checked
                                    ? [...new Set([...item.binding.allowed_source_scopes, source])]
                                    : item.binding.allowed_source_scopes.filter(value => value !== source) } } : item) }));
                    }} />{source}
                </label>)}
            </> : <>
                <p>{t('schedules.artifact.maxBytes')}: {input.max_content_bytes}</p>
                <p className="break-all">{t('schedules.contract.sources')}: {binding.allowed_source_scopes.join(' · ')}</p>
            </>}
        </section>;
    })}</>;
}
