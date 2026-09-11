import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { DeviceAssistantToolActivity } from './use-device-assistant-chat';

function formatPayload(value: string) {
    try { return JSON.stringify(JSON.parse(value), null, 2); }
    catch { return value; }
}

export function AssistantToolCall({ tool, running }: { tool?: DeviceAssistantToolActivity; running: boolean }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    if (!tool) return null;
    const prefix = 'pages.deviceAssistant.toolCall.';
    return <details open={open} onToggle={event => setOpen(event.currentTarget.open)} className="min-w-0 rounded-md border bg-muted/30 px-3 py-2">
        <summary className="cursor-pointer select-none break-words text-sm [overflow-wrap:anywhere]">
            {t(`${prefix}title`)} · {tool.name === 'unknown' ? t(`${prefix}unknown`) : tool.name}
        </summary>
        {open && <div className="mt-3 min-w-0 space-y-3 text-xs">
            {tool.permissionReason && <p className="whitespace-pre-wrap break-words">{t('pages.deviceAssistant.permissionReasonLabel', { reason: tool.permissionReason })}</p>}
            <div><p className="mb-1 font-medium">{t(`${prefix}input`)}</p>
                <pre className="max-h-80 overflow-auto whitespace-pre-wrap [overflow-wrap:anywhere]">{tool.name === 'unknown' ? t(`${prefix}missingInput`) : formatPayload(tool.argumentsJson)}</pre></div>
            <div><p className="mb-1 font-medium">{t(`${prefix}output`)}</p>
                <pre className="max-h-80 overflow-auto whitespace-pre-wrap [overflow-wrap:anywhere]">{tool.output === null
                    ? t(`${prefix}${running && tool.status === 'running' ? 'waiting' : 'missing'}`)
                    : tool.output === '' ? t(`${prefix}empty`) : formatPayload(tool.output)}</pre></div>
        </div>}
    </details>;
}
