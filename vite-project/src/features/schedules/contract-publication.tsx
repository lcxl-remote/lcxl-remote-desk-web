import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementRequest, ScheduleManagementResponse, ScheduleView } from '@/services/types';
import { ScheduleClient, ScheduleRequestError } from './client';

type Review = Extract<ScheduleManagementResponse, { result: 'task_contract' }>;
type Confirmation = Extract<ScheduleManagementRequest, { operation: 'publish_task' }>;

export function ContractPublication({ client, review, connected, onPublished }: {
    client: ScheduleClient; review: Review; connected: boolean; onPublished: (task: ScheduleView) => void;
}) {
    const { t } = useTranslation();
    const [confirmation, setConfirmation] = useState<Confirmation | null>(null);
    const [accepted, setAccepted] = useState(false);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const mounted = useRef(true);
    const pending = useRef(false);
    useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
    const contract = review.contract;
    const eligible = connected && contract && review.task.kind === 'fresh_task'
        && ['draft', 'awaiting_authorization', 'paused'].includes(review.task.status)
        && !review.task.active_run_id && review.task.target_device_id
        && contract.target_device_id === review.task.target_device_id
        && contract.task_revision === review.task_revision && contract.prompt_sha256 === review.prompt_sha256
        && /^[a-f0-9]{64}$/.test(review.contract_sha256 ?? '');
    if (!eligible || !contract) return null;
    const perform = async (publish: boolean) => {
        if (pending.current) return;
        pending.current = true; setBusy(true); setError('');
        try {
            if (!publish) {
                const response = await client.request({ operation: 'get_task_rehearsal', schedule_id: review.task.schedule_id });
                if (!mounted.current) return;
                if (response.result !== 'task_rehearsal' || response.task.schedule_id !== review.task.schedule_id
                    || response.task.revision !== review.task.revision || response.rehearsal?.status !== 'completed'
                    || response.rehearsal.schedule_id !== review.task.schedule_id
                    || response.rehearsal.task_revision !== review.task_revision) throw new ScheduleRequestError('invalid');
                setConfirmation({ operation: 'publish_task', schedule_id: review.task.schedule_id,
                    expected_revision: review.task.revision, contract_revision: contract.contract_revision,
                    contract_sha256: review.contract_sha256!, rehearsal_run_id: response.rehearsal.rehearsal_id,
                    expires_at: null, client_publish_key: v4() });
            } else if (confirmation && accepted) {
                // Retain this exact request after uncertain delivery; retries cannot create another approval.
                const response = await client.request(confirmation);
                if (!mounted.current) return;
                if (response.result !== 'task' || response.task.schedule_id !== review.task.schedule_id) throw new ScheduleRequestError('invalid');
                onPublished(response.task);
            }
        } catch (err) {
            if (mounted.current) setError(err instanceof ScheduleRequestError && err.reason === 'server' ? err.message : t('schedules.publication.failed'));
        } finally {
            pending.current = false;
            if (mounted.current) setBusy(false);
        }
    };
    return <section className="space-y-3 border-t pt-4">
        <p>{t('schedules.publication.note')}</p>
        {error && <p role="alert">{error}</p>}
        {!confirmation ? <Button disabled={busy} onClick={() => void perform(false)}>{t('schedules.publication.prepare')}</Button> : <>
            <p>{t('schedules.publication.ready')}</p>
            <label className="flex gap-2 items-start">
                <input type="checkbox" checked={accepted} disabled={busy} onChange={event => setAccepted(event.target.checked)} />
                <span>{t('schedules.publication.accept')}</span>
            </label>
            <Button disabled={busy || !accepted} onClick={() => void perform(true)}>{t('schedules.publication.publish')}</Button>
        </>}
    </section>;
}
