import { useTranslation } from 'react-i18next';
import { LoaderCircle, Send, X } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AssistantComposerActions({ turnRunning, canStop, stopping, sendDisabled, rehearsal, onStop }: {
    turnRunning: boolean;
    canStop: boolean;
    stopping: boolean;
    sendDisabled: boolean;
    rehearsal: boolean;
    onStop: () => void;
}) {
    const { t } = useTranslation();
    const stopLabel = t(stopping ? 'pages.aiAssistant.stopping' : 'pages.aiAssistant.stop');
    const sendLabel = t(rehearsal ? 'schedules.rehearsal.begin' : 'pages.aiAssistant.send');
    return <>
        {(turnRunning || canStop || stopping) && <Button type="button" className="assistant-action assistant-primary-action"
            aria-label={stopLabel} onClick={onStop} disabled={!canStop || stopping}>
            {turnRunning || stopping ? <LoaderCircle aria-hidden="true" className="h-4 w-4 shrink-0 animate-spin motion-reduce:animate-none" />
                : <X aria-hidden="true" className="h-4 w-4 shrink-0" />}
            <span className="assistant-action-label">{stopLabel}</span>
        </Button>}
        {!turnRunning && !stopping && <Button type="submit" className="assistant-action assistant-primary-action"
            aria-label={sendLabel} disabled={sendDisabled}>
            <Send aria-hidden="true" className="h-4 w-4 shrink-0" />
            <span className="assistant-action-label">{sendLabel}</span>
        </Button>}
    </>;
}
