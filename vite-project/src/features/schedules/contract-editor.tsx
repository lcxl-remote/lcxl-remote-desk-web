import { ContractArtifacts } from './contract-artifacts';
import { validAttachmentPolicy } from './attachment-policy';
import { ContractSteps } from './contract-steps';
import { ContractMessageBoundaries } from './contract-message-boundaries';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementResponse } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';

type Review = Extract<ScheduleManagementResponse, { result: 'task_contract' }>;
type Contract = NonNullable<Review['contract']>;
export function ContractEditor({ client, review, connected, onSaved, onClose }: {
    client: ScheduleClient; review: Review; connected: boolean; onSaved: (review: Review) => void; onClose: () => void;
}) {
    const { t } = useTranslation();
    const [draft, setDraft] = useState<Contract>(() => structuredClone(review.contract!));
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const pending = useRef(false);
    const mounted = useRef(true);
    useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
    const save = async () => {
        if (!connected || pending.current) return;
        pending.current = true; setBusy(true); setError('');
        try {
            const response = await client.request({ operation: 'save_task_contract', expected_revision: review.task.revision, contract: draft });
            if (!mounted.current) return;
            if (response.result !== 'task_contract' || response.task.schedule_id !== review.task.schedule_id
                || response.contract?.schedule_id !== review.task.schedule_id) throw new ScheduleRequestError('invalid');
            onSaved(response);
        } catch (reason) {
            if (mounted.current) setError(reason instanceof ScheduleRequestError && reason.reason === 'server' ? reason.message : t('schedules.requestFailed'));
        } finally { pending.current = false; if (mounted.current) setBusy(false); }
    };
    const positive = (value: string) => {
        const number = Number(value);
        return Number.isSafeInteger(number) && number > 0 ? number : 0;
    };
    // Immediate form feedback only. Durable authorization remains server-owned.
    const scopeConflicts = draft.permissions.filter(rule => {
        const automatic = rule.automatic;
        const ceiling = rule.approval_ceiling;
        return !automatic.resources.length || !automatic.operations.length
            || !ceiling.resources.length || !ceiling.operations.length
            || automatic.resources.some(value => !ceiling.resources.includes(value))
            || automatic.operations.some(value => !ceiling.operations.includes(value))
            || automatic.export_destinations.some(value => !ceiling.export_destinations.some(target => JSON.stringify(target) === JSON.stringify(value)))
            || (Object.keys(automatic.limits) as (keyof typeof automatic.limits)[]).some(key => automatic.limits[key] > ceiling.limits[key])
            || ceiling.limits.max_calls > draft.budget.max_calls_per_run;
    });
    const emptySources = draft.steps.filter(step => (step.binding.kind === 'send_message' || step.binding.kind === 'produce_text_artifact') && !step.binding.allowed_source_scopes.length);
    const valid = !scopeConflicts.length && !emptySources.length && Object.values(draft.budget).every(value => Number.isSafeInteger(value) && value > 0)
        && draft.permissions.every(rule => (rule.input.kind !== 'generated_message'
            || (Number.isSafeInteger(rule.input.max_subject_bytes) && rule.input.max_subject_bytes >= 0
                && Number.isSafeInteger(rule.input.max_body_bytes) && rule.input.max_body_bytes > 0
                && validAttachmentPolicy(rule.input.attachment_policy))))
        && draft.permissions.every(rule => rule.input.kind !== 'generated_text_artifact'
            || (Number.isSafeInteger(rule.input.max_content_bytes) && rule.input.max_content_bytes > 0 && rule.input.max_content_bytes <= 65536))
        && draft.permissions.every(rule => [rule.automatic, rule.approval_ceiling].every(scope =>
            Object.values(scope.limits).every(value => Number.isSafeInteger(value) && value > 0)));
    return <form className="space-y-4 rounded-lg border p-4" onSubmit={event => { event.preventDefault(); if (valid) void save(); }}>
        <p>{t('schedules.editor.note')}</p>
        <fieldset disabled={!connected || busy} className="space-y-3">
            <label className="block">{t('schedules.editor.exception')}
                <select className="ml-2 rounded border bg-background p-2" value={draft.exception_mode}
                    onChange={event => setDraft(current => ({ ...current, exception_mode: event.target.value as Contract['exception_mode'] }))}>
                    <option value="deny">{t('schedules.contract.exception.deny')}</option>
                    <option value="request_approval">{t('schedules.contract.exception.request_approval')}</option>
                </select>
            </label>
            {(Object.keys(draft.budget) as (keyof Contract['budget'])[]).map(key => <label className="block" key={key}>
                {t(`schedules.editor.${key}`)}
                <input className="ml-2 rounded border bg-background p-2" type="number" min={1} step={1} required value={draft.budget[key] || ''}
                    onChange={event => setDraft(current => ({ ...current, budget: { ...current.budget, [key]: positive(event.target.value) } }))} />
            </label>)}
            {draft.permissions.map((rule, index) => <section key={rule.rule_id} className="space-y-2 rounded border p-3">
                <h3 className="font-semibold">{rule.tool_name}</h3>
                {(['automatic', 'approval_ceiling'] as const).map(kind => <div className="space-y-2" key={kind}>
                    <p>{t(`schedules.editor.${kind}`)}</p>
                    {(['resources', 'operations'] as const).map(field => <div key={field}>
                        <p>{t(`schedules.rehearsal.${field}`)}</p>
                        {review.contract!.permissions.find(original => original.rule_id === rule.rule_id)![kind][field].map(value => <label className="mr-3 inline-flex items-center gap-2 break-all" key={value}>
                            <input type="checkbox" disabled={field === 'resources' && (rule.input.kind === 'generated_message' || rule.input.kind === 'generated_text_artifact')} checked={rule[kind][field].includes(value)} onChange={event => setDraft(current => ({ ...current,
                                permissions: current.permissions.map((item, position) => position !== index ? item : { ...item,
                                    [kind]: { ...item[kind], [field]: event.target.checked ? [...item[kind][field], value] : item[kind][field].filter(existing => existing !== value) } }) }))} />
                            {value}
                        </label>)}
                    </div>)}
                    <div>
                        <p>{t('schedules.rehearsal.destinations')}</p>
                        {review.contract!.permissions.find(original => original.rule_id === rule.rule_id)![kind].export_destinations.map(destination => {
                            const identity = JSON.stringify(destination);
                            return <label className="flex items-center gap-2 break-all" key={identity}>
                                <input type="checkbox" disabled={rule.input.kind === 'generated_message'} checked={rule[kind].export_destinations.some(value => JSON.stringify(value) === identity)} onChange={event => {
                                    const checked = event.target.checked;
                                    setDraft(current => ({ ...current, permissions: current.permissions.map(item => item.rule_id !== rule.rule_id ? item : {
                                        ...item, [kind]: { ...item[kind], export_destinations: checked ? [...item[kind].export_destinations, destination]
                                            : item[kind].export_destinations.filter(value => JSON.stringify(value) !== identity) },
                                    }) }));
                                }} />{Object.values(destination).join(' · ')}
                            </label>;
                        })}
                    </div>
                    {(Object.keys(rule[kind].limits) as (keyof typeof rule.automatic.limits)[]).map(limit => <label className="block" key={limit}>
                        {t(`schedules.editor.${limit}`)}
                        <input className="ml-2 rounded border bg-background p-2" type="number" min={1} step={1} required value={rule[kind].limits[limit] || ''}
                            onChange={event => setDraft(current => ({ ...current, permissions: current.permissions.map((item, position) => position !== index ? item : {
                                ...item, [kind]: { ...item[kind], limits: { ...item[kind].limits, [limit]: positive(event.target.value) } },
                            }) }))} />
                    </label>)}
                </div>)}
                {draft.steps.some(step => step.rule_id === rule.rule_id)
                    ? <p className="text-sm">{t('schedules.editor.fixedRule')}</p>
                    : <Button type="button" variant="outline" onClick={() => setDraft(current => ({ ...current, permissions: current.permissions.filter(item => item.rule_id !== rule.rule_id) }))}>{t('schedules.editor.removeRule')}</Button>}
            </section>)}
            <ContractSteps draft={draft} setDraft={setDraft} />
            <ContractArtifacts contract={draft} original={review.contract!} setDraft={setDraft} />
            <ContractMessageBoundaries draft={draft} original={review.contract!} setDraft={setDraft} />
        </fieldset>
        {scopeConflicts.length > 0 && <div role="alert">
            <p>{t('schedules.editor.scopeConflict')}</p>
            <ul className="list-inside list-disc">{scopeConflicts.map(rule => <li key={rule.rule_id}>{rule.tool_name}</li>)}</ul>
        </div>}
        {emptySources.length > 0 && <p role="alert">{t('schedules.editor.sourceRequired')}</p>}
        {!valid && !scopeConflicts.length && !emptySources.length && <p role="alert">{t('schedules.editor.invalidNumbers')}</p>}
        {error && <p role="alert">{error}</p>}
        <div className="flex gap-2">
            <Button type="submit" disabled={!connected || busy || !valid}>{t('schedules.editor.save')}</Button>
            <Button type="button" variant="outline" disabled={busy} onClick={onClose}>{t('schedules.editor.cancel')}</Button>
        </div>
    </form>;
}
