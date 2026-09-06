import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';

type CommandReceipt = {
    exit_code: number;
    duration_ms: number;
    streams: { type: 'split'; stdout: string; stderr: string; stdout_truncated: boolean; stderr_truncated: boolean }
        | { type: 'pty_combined'; terminal: string; truncated: boolean };
    redactions?: string[];
};

function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function parseCommandReceipt(text: string): CommandReceipt | null {
    try {
        const envelope: unknown = JSON.parse(text);
        if (!isRecord(envelope) || !isRecord(envelope.Exec)) return null;
        const value = envelope.Exec;
        const streams = value.streams;
        if (!Number.isSafeInteger(value.exit_code) || !Number.isSafeInteger(value.duration_ms)
            || (value.duration_ms as number) < 0 || !isRecord(streams)) return null;
        if (value.redactions !== undefined && (!Array.isArray(value.redactions)
            || !value.redactions.every(item => typeof item === 'string'))) return null;
        if (streams.type === 'split') {
            if (typeof streams.stdout !== 'string' || typeof streams.stderr !== 'string'
                || typeof streams.stdout_truncated !== 'boolean' || typeof streams.stderr_truncated !== 'boolean') return null;
        } else if (streams.type === 'pty_combined') {
            if (typeof streams.terminal !== 'string' || typeof streams.truncated !== 'boolean') return null;
        } else return null;
        return value as CommandReceipt;
    } catch {
        return null;
    }
}

export function AssistantCommandResult({ text }: { text: string }) {
    const { t, i18n } = useTranslation();
    const receipt = useMemo(() => parseCommandReceipt(text), [text]);
    const output = (label: string, content: string, truncated: boolean) => (
        <div className="min-w-0 space-y-1">
            <p className="font-medium">{label}</p>
            <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded bg-background p-2 text-xs">{content || t('pages.deviceAssistant.commandReceipt.empty')}</pre>
            {truncated && <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.commandReceipt.truncated')}</p>}
        </div>
    );
    return (
        <details className="min-w-0">
            <summary className="cursor-pointer rounded font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
                {t('pages.deviceAssistant.commandResultTitle')}
            </summary>
            <div className="mt-3 space-y-3">
                {receipt ? <>
                    <dl className="flex flex-wrap gap-x-6 gap-y-2">
                        <div><dt className="text-muted-foreground">{t('pages.deviceAssistant.commandReceipt.exitCode')}</dt><dd>{receipt.exit_code}</dd></div>
                        <div><dt className="text-muted-foreground">{t('pages.deviceAssistant.commandReceipt.duration')}</dt><dd>{t('pages.deviceAssistant.commandReceipt.milliseconds', { value: receipt.duration_ms.toLocaleString(i18n.language) })}</dd></div>
                    </dl>
                    {receipt.streams.type === 'split' ? <>
                        {output(t('pages.deviceAssistant.commandReceipt.stdout'), receipt.streams.stdout, receipt.streams.stdout_truncated)}
                        {output(t('pages.deviceAssistant.commandReceipt.stderr'), receipt.streams.stderr, receipt.streams.stderr_truncated)}
                    </> : output(t('pages.deviceAssistant.commandReceipt.terminal'), receipt.streams.terminal, receipt.streams.truncated)}
                    {!!receipt.redactions?.length && output(t('pages.deviceAssistant.commandReceipt.redactions'), receipt.redactions.join('\n'), false)}
                    <details>
                        <summary className="cursor-pointer text-xs text-muted-foreground">{t('pages.deviceAssistant.commandReceipt.raw')}</summary>
                        <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{text}</pre>
                    </details>
                </> : <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words text-xs">{text}</pre>}
                <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.commandResultHint')}</p>
            </div>
        </details>
    );
}
