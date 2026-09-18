import { useTranslation } from 'react-i18next';
import { Disclosure } from '@/components/ui/disclosure';

type LaunchReceipt = {
    launch_outcome: 'launch_accepted' | 'not_dispatched' | 'launch_failed' | 'outcome_unknown';
    argument_delivery: 'not_requested' | 'submitted' | 'unsupported' | 'unknown';
    requested_admin: boolean;
    created_process_id?: number | null;
    created_process_elevated?: boolean | null;
    failure_reason?: string | null;
};
const record = (value: unknown): value is Record<string, unknown> => Boolean(value) && typeof value === 'object' && !Array.isArray(value);
const reasons = ['invalid_target', 'identity_changed', 'elevation_required', 'admin_required', 'admin_launch_unavailable',
    'unsupported', 'session_unavailable', 'lifetime_isolation_unavailable', 'permission_denied', 'native_failure'];
export function parseLaunchReceipt(text: string): LaunchReceipt | null {
    try {
        const completion: unknown = JSON.parse(text);
        if (!record(completion) || !record(completion.output) || completion.output.kind !== 'application_launch'
            || !record(completion.output.value)) return null;
        const value = completion.output.value;
        const classes: Record<string, string> = { launch_accepted: 'changed_but_unverified', not_dispatched: 'definitely_not_started',
            launch_failed: 'definitely_not_started', outcome_unknown: 'outcome_unknown' };
        if (typeof value.launch_outcome !== 'string' || !Object.hasOwn(classes, value.launch_outcome)
            || completion.result !== classes[value.launch_outcome]
            || typeof value.requested_admin !== 'boolean'
            || typeof value.argument_delivery !== 'string' || !['not_requested', 'submitted', 'unsupported', 'unknown'].includes(value.argument_delivery)
            || (value.failure_reason != null && (typeof value.failure_reason !== 'string' || !reasons.includes(value.failure_reason)))
            || (value.created_process_id != null && (!Number.isSafeInteger(value.created_process_id) || (value.created_process_id as number) <= 0 || (value.created_process_id as number) > 4294967295))
            || (value.created_process_elevated != null && (typeof value.created_process_elevated !== 'boolean' || value.created_process_id == null))
            || (value.created_process_elevated === true && value.requested_admin !== true)
            || (value.launch_outcome === 'launch_accepted' && (value.failure_reason != null || value.argument_delivery === 'unsupported'))
            || (value.launch_outcome !== 'launch_accepted' && value.created_process_id != null)) return null;
        return value as LaunchReceipt;
    } catch { return null; }
}
export function AssistantLaunchResult({ receipt, text }: { receipt: LaunchReceipt; text: string }) {
    const { t } = useTranslation();
    return <Disclosure className="min-w-0" title={<>{t('pages.aiAssistant.launchReceipt.title')} · {t(`pages.aiAssistant.launchReceipt.${receipt.launch_outcome}`)}</>} summaryClassName="cursor-pointer font-medium">
        <div className="mt-3 space-y-2 text-sm">
            <p>{t('pages.aiAssistant.launchReceipt.readiness')}</p>
            {receipt.launch_outcome === 'outcome_unknown' && <p className="font-medium text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.launchReceipt.noRetry')}</p>}
            {receipt.failure_reason && <p>{t(`pages.aiAssistant.launchFailure.${receipt.failure_reason}`)}</p>}
            <p>{t(`pages.aiAssistant.launchArguments.${receipt.argument_delivery}`)}</p>
            <p>{t(receipt.requested_admin ? 'pages.aiAssistant.launchReceipt.requestedAdmin' : 'pages.aiAssistant.launchReceipt.requestedUser')}</p>
            {receipt.created_process_id != null && <p>{t('pages.aiAssistant.launchReceipt.pid', { pid: receipt.created_process_id })}</p>}
            {receipt.created_process_elevated != null && <p>{t(receipt.created_process_elevated ? 'pages.aiAssistant.launchReceipt.createdAdmin' : 'pages.aiAssistant.launchReceipt.createdUser')}</p>}
        </div>
        <Disclosure className="mt-3" title={<>{t('pages.aiAssistant.commandReceipt.raw')}</>} summaryClassName="cursor-pointer text-xs"><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all text-xs">{text}</pre></Disclosure>
    </Disclosure>;
}
