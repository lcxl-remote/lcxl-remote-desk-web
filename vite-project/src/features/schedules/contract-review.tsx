import { ContractArtifacts } from './contract-artifacts';
import { ContractAttachments } from './contract-attachments';
import { ContractEditor } from './contract-editor';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementResponse, ScheduleView } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';
import { ContractPublication } from './contract-publication';

type Review = Extract<ScheduleManagementResponse, { result: 'task_contract' }>;
export function ContractReview({ client, scheduleId, connected, onPublished }: { client: ScheduleClient; scheduleId: string; connected: boolean; onPublished?: (task: ScheduleView) => void }) {
    const { t } = useTranslation();
    const [review, setReview] = useState<Review | null>(null);
    const [error, setError] = useState('');
    const [editing, setEditing] = useState(false);
    const [regenerating, setRegenerating] = useState(false);
    const [loading, setLoading] = useState(false);
    const [refresh, setRefresh] = useState(0);
    const generation = useRef(0);
    useEffect(() => {
        let active = true;
        ++generation.current;
        setReview(null); setEditing(false); setRegenerating(false); setError(''); setLoading(connected);
        if (connected) void client.request({ operation: 'get_task_contract', schedule_id: scheduleId }).then(response => {
            if (!active) return;
            if (response.result !== 'task_contract' || response.task.schedule_id !== scheduleId ||
                (response.contract && response.contract.schedule_id !== scheduleId)) throw new ScheduleRequestError('invalid');
            setReview(response);
        }).catch(err => {
            if (active) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
        }).finally(() => { if (active) setLoading(false); });
        return () => { active = false; ++generation.current; };
    }, [client, scheduleId, connected, refresh, t]);
    const contract = review?.contract;
    const generate = async () => {
        if (!review || loading || !connected || editing || review.task.active_run_id
            || !['draft', 'awaiting_authorization', 'paused'].includes(review.task.status) || (contract && !regenerating)) return;
        const current = generation.current;
        setLoading(true); setError('');
        try {
            const response = await client.request({ operation: 'generate_task_contract', schedule_id: scheduleId, expected_revision: review.task.revision });
            if (current !== generation.current) return;
            if (response.result !== 'task_contract' || response.task.schedule_id !== scheduleId || response.contract?.schedule_id !== scheduleId) throw new ScheduleRequestError('invalid');
            setReview(response); setRegenerating(false); onPublished?.(response.task);
        } catch (err) {
            if (current === generation.current) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.requestFailed'));
        } finally { if (current === generation.current) setLoading(false); }
    };
    return <div className="space-y-4">
        <p>{t('schedules.editor.reviewNote')}</p>
        <Button variant="outline" disabled={!connected || loading} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>
        {!connected && <p role="status">{t('schedules.connecting')}</p>}
        {loading && <p role="status">{t('schedules.loading')}</p>}
        {error && <p role="alert">{error}</p>}
        {review && <section className="space-y-1 border-b pb-3">
            <h3>{t('schedules.authorization.title')}</h3>
            {review.authorization ? <>
                <p>{t('schedules.authorization.versions', { authorization: review.authorization.authorization_revision, contract: review.authorization.contract_revision, task: review.authorization.task_revision })}</p>
                <p>{t('schedules.authorization.approved')}: {new Date(review.authorization.approved_at).toLocaleString()}</p>
                <p>{t('schedules.authorization.expires')}: {review.authorization.expires_at ? new Date(review.authorization.expires_at).toLocaleString() : t('schedules.authorization.noExpiry')}</p>
                {review.authorization.revoked_at && <p>{t('schedules.authorization.revoked')}: {new Date(review.authorization.revoked_at).toLocaleString()}</p>}
            </> : <p>{t('schedules.authorization.none')}</p>}
            <p className="text-sm text-muted-foreground">{t('schedules.authorization.note')}</p>
        </section>}
        {review && !contract && <p>{t('schedules.contract.empty')}</p>}
        {review && !contract && review.task.kind === 'fresh_task' && !review.task.active_run_id && ['draft', 'awaiting_authorization', 'paused'].includes(review.task.status) && <Button disabled={!connected || loading} onClick={() => void generate()}>{t('schedules.contract.generate')}</Button>}
        {review && contract && connected && !loading && !editing && !review.task.active_run_id && ['draft', 'awaiting_authorization', 'paused'].includes(review.task.status) &&
            <div className="space-y-2">
                <Button variant="outline" onClick={() => setRegenerating(value => !value)}>{t('schedules.editor.regenerate')}</Button>
                {regenerating && <div className="space-y-2 rounded border p-3">
                    <p>{t('schedules.editor.regenerateNote')}</p>
                    <Button onClick={() => void generate()}>{t('schedules.editor.confirmRegenerate')}</Button>
                </div>}
            </div>}
        {review && contract && connected && !loading && !review.task.active_run_id && ['draft', 'awaiting_authorization', 'paused'].includes(review.task.status) && !editing &&
            <Button variant="outline" onClick={() => { setRegenerating(false); setEditing(true); }}>{t('schedules.editor.edit')}</Button>}
        {review && contract && connected && editing && <ContractEditor key={`${scheduleId}:${review.task.revision}:${refresh}`} client={client} review={review} connected={connected}
            onClose={() => setEditing(false)} onSaved={response => { setReview(response); setEditing(false); onPublished?.(response.task); }} />}
        {contract && !editing && <>
            {review && <p role="status">{t(`schedules.status.${review.task.status}`)}</p>}
            <ContractContents contract={contract} />
            {review?.previous_contract && <section className="space-y-2">
                <h3>{t('schedules.diff.title')}</h3>
                {contractChanges.filter(key => canonicalValue(contract[key]) !== canonicalValue(review.previous_contract![key])).map(key =>
                    <p key={key}>{t(`schedules.diff.${key}`)}</p>)}
                <details><summary>{t('schedules.diff.previous')}</summary><ContractContents contract={review.previous_contract} /></details>
            </section>}
        </>}
        {review && !editing && !regenerating && !loading && <ContractPublication key={`${review.task.revision}:${review.contract_sha256}:${connected}`} client={client} review={review} connected={connected}
            onPublished={task => { setReview(current => current ? { ...current, task } : null); onPublished?.(task); }} />}
    </div>;
}

const contractChanges = ['budget', 'permissions', 'steps', 'exception_mode', 'prompt_sha256'] as const;
function canonicalValue(value: unknown): string {
    if (Array.isArray(value)) return `[${value.map(canonicalValue).join(',')}]`;
    if (value !== null && typeof value === 'object') return `{${Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, item]) => `${JSON.stringify(key)}:${canonicalValue(item)}`).join(',')}}`;
    return JSON.stringify(value) ?? 'null';
}
function ContractContents({ contract }: { contract: NonNullable<Review['contract']> }) {
    const { t } = useTranslation();
    return <>

            <p>{t('schedules.contract.version', { version: contract.contract_revision })}</p>
            <p>{t(`schedules.contract.exception.${contract.exception_mode}`)}</p>
            <p>{t('schedules.contract.runs', { count: contract.budget.max_runs_per_utc_day })}</p>
            <p>{t('schedules.contract.calls', { count: contract.budget.max_calls_per_run })}</p>
            <p>{t('schedules.contract.tokens', { count: contract.budget.max_model_tokens_per_run })}</p>
            <p>{t('schedules.contract.seconds', { count: contract.budget.max_runtime_seconds })}</p>
            {contract.steps.map(step => step.binding.kind === 'send_message' && <article className="rounded-md border p-3 space-y-2" key={step.step_id}>
                <p>{t('schedules.contract.account')}: {step.binding.destination.account_id}</p>
                <p>{t('schedules.contract.profile')}: {step.binding.destination.profile_id}</p>
                <p>{t('schedules.rehearsal.resources')}: {step.binding.destination.scope && (step.binding.destination.scope.kind === 'web_origin' ? `${step.binding.destination.scope.origin.kind} ${step.binding.destination.scope.origin.host_ascii}:${step.binding.destination.scope.origin.port}` : step.binding.destination.scope.application_id)} · {step.binding.destination.channel}</p>
                <p>{t('schedules.contract.sources')}: {step.binding.allowed_source_scopes?.join(' · ')}</p>
                {step.binding.destination.recipients.map(recipient => <p className="break-all" key={`${recipient.role}:${recipient.stable_id}`}>{t('schedules.contract.recipient')}: {recipient.role} · {recipient.canonical_address} · {recipient.stable_id}</p>)}
            </article>)}
            <ContractArtifacts contract={contract} />
            {contract.permissions.map(rule => <article className="rounded-md border p-3 space-y-2" key={rule.rule_id}>
                <h3 className="font-semibold break-all">{rule.tool_name}</h3>
                <p className="break-all">{t('schedules.rehearsal.resources')}: {rule.automatic.resources.join(' · ')}</p>
                <p className="break-all">{t('schedules.rehearsal.operations')}: {rule.automatic.operations.join(' · ')}</p>
                {rule.automatic.limits && <p>{t('schedules.contract.limits', { bytes: rule.automatic.limits.max_bytes_per_call, items: rule.automatic.limits.max_items_per_call, calls: rule.automatic.limits.max_calls })}</p>}
                {!!rule.automatic.export_destinations?.length && <p className="break-all">{t('schedules.rehearsal.destinations')}: {rule.automatic.export_destinations.map(item => Object.values(item).join(' · ')).join('; ')}</p>}
                {rule.input?.kind === 'exact' && <div><p>{t('schedules.contract.exactInput')}</p><pre className="whitespace-pre-wrap break-all">{rule.input.canonical_json}</pre></div>}
                {rule.input?.kind === 'scoped_read' && <p>{t('schedules.contract.scopedRead')}</p>}
                {rule.input?.kind === 'generated_message' && <ContractAttachments policy={rule.input.attachment_policy} />}
                {rule.input?.kind === 'generated_message' && <p>{t('schedules.contract.generated', { subject: rule.input.max_subject_bytes, body: rule.input.max_body_bytes })}</p>}
                {contract.exception_mode === 'request_approval' && rule.approval_ceiling && <div>
                    <p>{t('schedules.contract.ceiling')}</p>
                    <p className="break-all">{rule.approval_ceiling.resources.join(' · ')}; {rule.approval_ceiling.operations.join(' · ')}</p>
                    <p>{t('schedules.contract.limits', { bytes: rule.approval_ceiling.limits.max_bytes_per_call, items: rule.approval_ceiling.limits.max_items_per_call, calls: rule.approval_ceiling.limits.max_calls })}</p>
                    {!!rule.approval_ceiling.export_destinations.length && <p className="break-all">{t('schedules.rehearsal.destinations')}: {rule.approval_ceiling.export_destinations.map(item => Object.values(item).join(' · ')).join('; ')}</p>}
                </div>}
            </article>)}
            </>;
}
