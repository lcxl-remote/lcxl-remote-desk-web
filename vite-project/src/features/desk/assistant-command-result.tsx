import type { AiAssistantToolActivity } from './use-ai-assistant-chat';
import { Button } from '@/components/ui/button';
import { AssistantCodeBlock } from './assistant-code-block';
import { Disclosure } from '@/components/ui/disclosure';
import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { AssistantFileResult, parseFileReceipt } from './assistant-file-result';
import { hasUnknownActionResult } from './action-result-status';
import { AssistantLaunchResult, parseLaunchReceipt } from './assistant-launch-result';

type CommandReceipt = {
    exit_code: number | null;
    failure?: { message: string; kind: string } | null;
    termination_signal?: number | null;
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
        if ((value.exit_code !== null && !Number.isSafeInteger(value.exit_code)) || !Number.isSafeInteger(value.duration_ms)
            || (value.duration_ms as number) < 0 || !isRecord(streams)) return null;
        if (value.failure != null && (!isRecord(value.failure) || typeof value.failure.message !== 'string' || typeof value.failure.kind !== 'string')) return null;
        if (value.termination_signal != null && !Number.isSafeInteger(value.termination_signal)) return null;
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

export function AssistantCommandResult({ text, tool, onLocateCall, onExportBackup }: {
    text: string;
    tool?: AiAssistantToolActivity;
    onLocateCall?: () => void;
    onExportBackup?: (id: string) => Promise<void>;
}) {
    const { t, i18n } = useTranslation();
    const command = useMemo(() => {
        if (tool?.name !== 'exec_command') return null;
        try {
            const args: unknown = JSON.parse(tool.argumentsJson);
            return isRecord(args) && typeof args.command === 'string' && typeof args.shell === 'string'
                ? { text: args.command, shell: args.shell } : null;
        } catch { return null; }
    }, [tool]);
    const receipt = useMemo(() => parseCommandReceipt(text), [text]);
    const fileReceipt = useMemo(() => parseFileReceipt(text), [text]);
    const launchReceipt = useMemo(() => parseLaunchReceipt(text), [text]);
    const outcomeUnknown = useMemo(() => hasUnknownActionResult(text), [text]);
    if (launchReceipt) return <AssistantLaunchResult receipt={launchReceipt} text={text} />;
    if (fileReceipt) return <AssistantFileResult receipt={fileReceipt} text={text} onExportBackup={onExportBackup} />;
    const output = (label: string, content: string, truncated: boolean) => (
        <div className="min-w-0 space-y-1">
            <AssistantCodeBlock label={label} text={content || t('pages.aiAssistant.commandReceipt.empty')} format="text" />
            {truncated && <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.commandReceipt.truncated')}</p>}
        </div>
    );
    const duration = receipt ? t(`pages.aiAssistant.commandReceipt.${receipt.duration_ms >= 60000 ? 'minutes' : receipt.duration_ms >= 1000 ? 'seconds' : 'milliseconds'}`, {
        value: (receipt.duration_ms / (receipt.duration_ms >= 60000 ? 60000 : receipt.duration_ms >= 1000 ? 1000 : 1)).toLocaleString(i18n.language, { maximumFractionDigits: 2 }),
    }) : '';
    return (
        <div className="min-w-0 space-y-2">
            {command && <div className="min-w-0 space-y-1">
                <p className="text-xs text-muted-foreground">{command.shell}</p>
                <p className="truncate font-mono text-xs" title={command.text}>{command.text.split(/\r?\n/).find(line => line.trim())?.slice(0, 160)}</p>
                <Disclosure title={t('pages.aiAssistant.commandReceipt.command')}>
                    <AssistantCodeBlock text={command.text} format="text" />
                </Disclosure>
                {onLocateCall && <Button variant="link" size="sm" className="h-auto p-0" onClick={onLocateCall}>{t('pages.aiAssistant.commandReceipt.locate')}</Button>}
            </div>}
            <Disclosure className="min-w-0" title={<>
                    {t('pages.aiAssistant.commandResultTitle')}
                    {outcomeUnknown && <> · {t('pages.aiAssistant.toolCall.inspectBeforeRetry')}</>}
                </>} summaryClassName="cursor-pointer rounded font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">

                <div className="mt-3 space-y-3">
                    {receipt ? <>
                        <dl className="flex flex-wrap gap-x-6 gap-y-2">
                            <div><dt className="text-muted-foreground">{t('pages.aiAssistant.commandReceipt.exitCode')}</dt><dd>{receipt.exit_code ?? '—'}</dd></div>
                            <div><dt className="text-muted-foreground">{t('pages.aiAssistant.commandReceipt.duration')}</dt><dd>{duration}</dd></div>
                        </dl>
                        {receipt.failure && <p role="alert" className="whitespace-pre-wrap break-words text-destructive">{receipt.failure.message}</p>}
                        {receipt.streams.type === 'split' ? <>
                            {output(t('pages.aiAssistant.commandReceipt.stdout'), receipt.streams.stdout, receipt.streams.stdout_truncated)}
                            {output(t('pages.aiAssistant.commandReceipt.stderr'), receipt.streams.stderr, receipt.streams.stderr_truncated)}
                        </> : output(t('pages.aiAssistant.commandReceipt.terminal'), receipt.streams.terminal, receipt.streams.truncated)}
                        {!!receipt.redactions?.length && output(t('pages.aiAssistant.commandReceipt.redactions'), receipt.redactions.join('\n'), false)}
                        <Disclosure title={<>{t('pages.aiAssistant.commandReceipt.raw')}</>} summaryClassName="cursor-pointer text-xs text-muted-foreground">

                            <AssistantCodeBlock text={text} />
                        </Disclosure>
                    </> : <AssistantCodeBlock text={text} />}
                    <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.commandResultHint')}</p>
                </div>
            </Disclosure>
        </div>
    );
}
