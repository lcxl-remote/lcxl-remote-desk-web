import { Sparkles } from 'lucide-react';
import { useTranslation } from 'react-i18next';

export function AssistantConnectionIcon({ connected, enabled }: { connected: boolean; enabled: boolean }) {
    const { t } = useTranslation();
    const label = t(!enabled ? 'pages.deviceAssistant.disabledTitle'
        : connected ? 'pages.deviceAssistant.signalConnected' : 'pages.deviceAssistant.signalConnecting');
    const color = !enabled ? 'text-muted-foreground' : connected ? 'text-green-500' : 'text-amber-500';
    return <span role="status" title={label} tabIndex={0} data-testid="assistant-connection-icon"
        className="inline-flex shrink-0 rounded focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
        <Sparkles aria-hidden="true" className={`h-4 w-4 ${color}`} />
        <span className="sr-only">{label}</span>
    </span>;
}
