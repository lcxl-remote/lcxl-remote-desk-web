import { useTranslation } from 'react-i18next';

export interface CommandReview {
    command: string;
    shell: string;
    cwd?: string | null;
    targetDeviceId: string;
    targetSessionId: string;
    timeoutMs: number;
    maxStdoutBytes: number;
    maxStderrBytes: number;
    executionBasis: string;
    oneShot: boolean;
}

export function validCommandReview(input: unknown): input is CommandReview {
    if (!input || typeof input !== 'object') return false;
    const value = input as CommandReview;
    return Boolean(typeof value.command === 'string' && value.command.trim()
        && typeof value.shell === 'string' && ['powershell', 'pwsh', 'bash', 'sh'].includes(value.shell)
        && typeof value.targetDeviceId === 'string' && value.targetDeviceId.trim()
        && typeof value.targetSessionId === 'string' && value.targetSessionId.trim() && value.oneShot === true
        && (value.cwd == null || (typeof value.cwd === 'string' && value.cwd.length > 0))
        && Number.isInteger(value.timeoutMs) && value.timeoutMs > 0
        && Number.isInteger(value.maxStdoutBytes) && value.maxStdoutBytes > 0
        && Number.isInteger(value.maxStderrBytes) && value.maxStderrBytes > 0
        && ['template', 'owner_blocklist_only'].includes(value.executionBasis));
}

export function CommandConfirmationCard({ value }: { value: CommandReview }) {
    const { t } = useTranslation();
    return (
        <div className="mt-3 space-y-2 rounded-md border p-3 text-xs" data-testid="command-confirmation">
            <p className="font-semibold">{t('pages.deviceAssistant.commandConfirmTitle')}</p>
            <p>{t(value.executionBasis === 'owner_blocklist_only'
                ? 'pages.deviceAssistant.commandFreeformWarning'
                : 'pages.deviceAssistant.commandOneShot')}</p>
            <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2">{value.command}</pre>
            <dl className="grid grid-cols-[max-content_1fr] gap-x-3 gap-y-1">
                <dt>{t('pages.deviceAssistant.commandShell')}</dt><dd>{value.shell}</dd>
                <dt>{t('pages.deviceAssistant.commandCwd')}</dt><dd className="break-all">{value.cwd ?? t('pages.deviceAssistant.commandDefaultCwd')}</dd>
                <dt>{t('pages.deviceAssistant.commandTarget')}</dt><dd className="break-all">{value.targetDeviceId} / {value.targetSessionId}</dd>
                <dt>{t('pages.deviceAssistant.commandLimits')}</dt><dd>{t('pages.deviceAssistant.commandLimitsValue', {
                    seconds: value.timeoutMs / 1000, stdout: value.maxStdoutBytes, stderr: value.maxStderrBytes,
                })}</dd>
            </dl>
        </div>
    );
}
