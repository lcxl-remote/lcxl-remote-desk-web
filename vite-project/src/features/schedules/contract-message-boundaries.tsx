import { ContractAttachments } from './contract-attachments';
import { useTranslation } from 'react-i18next';
import type { Dispatch, SetStateAction } from 'react';
import type { ScheduleManagementResponse } from '@/services/types';

type Contract = NonNullable<Extract<ScheduleManagementResponse, { result: 'task_contract' }>['contract']>;
export function ContractMessageBoundaries({ draft, original, setDraft }: {
    draft: Contract; original: Contract; setDraft: Dispatch<SetStateAction<Contract>>;
}) {
    const { t } = useTranslation();
    return <div className="space-y-3">
        {draft.permissions.map(rule => rule.input.kind === 'generated_message' && <section className="space-y-2 rounded border p-3" key={rule.rule_id}>
            <h3 className="font-semibold">{rule.tool_name}</h3>
            <ContractAttachments policy={rule.input.attachment_policy} original={(() => { const input = original.permissions.find(item => item.rule_id === rule.rule_id)?.input; return input?.kind === 'generated_message' ? input.attachment_policy : undefined; })()}
                onChange={policy => setDraft(current => ({ ...current, permissions: current.permissions.map(item => item.rule_id === rule.rule_id && item.input.kind === 'generated_message'
                    ? { ...item, input: { ...item.input, attachment_policy: policy } } : item) }))} />
            {(['max_subject_bytes', 'max_body_bytes'] as const).map(field => <label className="block" key={field}>
                {t(`schedules.editor.${field}`)}
                <input className="ml-2 rounded border bg-background p-2" type="number" min={field === 'max_subject_bytes' ? 0 : 1} step={1} required
                    value={rule.input.kind === 'generated_message' ? rule.input[field] : 0}
                    onChange={event => {
                        const value = Number(event.target.value);
                        setDraft(current => ({ ...current, permissions: current.permissions.map(item => item.rule_id === rule.rule_id && item.input.kind === 'generated_message'
                            ? { ...item, input: { ...item.input, [field]: Number.isSafeInteger(value) && value >= 0 ? value : 0 } } : item) }));
                    }} />
            </label>)}
        </section>)}
        {draft.steps.map(step => {
            if (step.binding.kind !== 'send_message') return null;
            const binding = step.binding;
            const previous = original.steps.find(item => item.step_id === step.step_id)?.binding;
            return <section className="space-y-2 rounded border p-3" key={step.step_id}>
                <h3 className="font-semibold">{t('schedules.editor.messageStep', { step: step.step_id })}</h3>
                <p>{t('schedules.contract.account')}: {binding.destination.account_id}</p>
                <p>{t('schedules.contract.profile')}: {binding.destination.profile_id}</p>
                {binding.destination.recipients.map(recipient => <p className="break-all" key={`${recipient.role}:${recipient.stable_id}`}>
                    {t('schedules.contract.recipient')}: {recipient.role} · {recipient.canonical_address} · {recipient.stable_id}
                </p>)}
                <p className="text-sm">{t('schedules.editor.destinationFixed')}</p>
                <p>{t('schedules.contract.sources')}</p>
                {previous?.kind === 'send_message' && previous.allowed_source_scopes.map(source => <label className="flex items-center gap-2 break-all" key={source}>
                    <input type="checkbox" checked={binding.allowed_source_scopes.includes(source)} onChange={event => {
                        const checked = event.target.checked;
                        setDraft(current => ({ ...current, steps: current.steps.map(item => item.step_id === step.step_id && item.binding.kind === 'send_message'
                            ? { ...item, binding: { ...item.binding, allowed_source_scopes: checked
                                ? [...item.binding.allowed_source_scopes, source]
                                : item.binding.allowed_source_scopes.filter(value => value !== source) } } : item) }));
                    }} />{source}
                </label>)}
            </section>;
        })}
    </div>;
}
