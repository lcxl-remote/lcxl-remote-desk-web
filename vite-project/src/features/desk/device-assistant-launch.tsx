import { useTranslation } from 'react-i18next';

export interface LaunchReview {
    target: { kind: string; value: string };
    resolvedTarget: string;
    args: string[];
    cwd?: string | null;
    runAsAdmin: boolean;
    targetDeviceId: string;
    targetSessionId: string;
    oneShot: boolean;
}

export function validLaunchReview(input: unknown): input is LaunchReview {
    if (!input || typeof input !== 'object') return false;
    const value = input as LaunchReview;
    const text = (entry: unknown) => typeof entry === 'string' && entry.trim().length > 0 && !entry.includes('\0');
    return Boolean(value.target && ['executable', 'macos_bundle', 'windows_app_id'].includes(value.target.kind)
        && text(value.target.value) && text(value.resolvedTarget)
        && Array.isArray(value.args) && value.args.length <= 256
        && value.args.every((arg) => typeof arg === 'string' && !arg.includes('\0') && arg.length <= 16384)
        && (value.cwd == null || text(value.cwd))
        && typeof value.runAsAdmin === 'boolean' && value.oneShot === true
        && text(value.targetDeviceId) && text(value.targetSessionId));
}

export function LaunchConfirmationCard({ value }: { value: LaunchReview }) {
    const { t } = useTranslation();
    return <div className="mt-3 space-y-2 rounded-md border p-3 text-xs" data-testid="launch-confirmation">
        <p className="font-semibold">{t('pages.deviceAssistant.launchConfirmTitle')}</p>
        <p>{t('pages.deviceAssistant.launchLifetime')}</p>
        <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1">
            <dt>{t('pages.deviceAssistant.launchTarget')}</dt><dd className="break-all">{value.target.value}</dd>
            <dt>{t('pages.deviceAssistant.launchResolvedTarget')}</dt><dd className="break-all">{value.resolvedTarget}</dd>
            <dt>{t('pages.deviceAssistant.commandCwd')}</dt><dd className="break-all">{value.cwd ?? t('pages.deviceAssistant.launchDefaultCwd')}</dd>
            <dt>{t('pages.deviceAssistant.commandTarget')}</dt><dd className="break-all">{value.targetDeviceId} / {value.targetSessionId}</dd>
            <dt>{t('pages.deviceAssistant.launchPrivilege')}</dt><dd>{t(value.runAsAdmin ? 'pages.deviceAssistant.launchAdmin' : 'pages.deviceAssistant.launchUser')}</dd>
        </dl>
        {value.runAsAdmin && <p className="font-medium text-amber-700 dark:text-amber-300">{t('pages.deviceAssistant.launchAdminWarning')}</p>}
        <p className="font-medium">{t('pages.deviceAssistant.launchArgs')}</p>
        <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2" data-testid="launch-arguments">{JSON.stringify(value.args, null, 2)}</pre>
    </div>;
}
