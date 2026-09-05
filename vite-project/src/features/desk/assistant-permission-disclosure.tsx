import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, ChevronRight } from 'lucide-react';
import { Badge } from '@/components/ui/badge';

const completedStates = new Set(['approved', 'partially_approved', 'denied', 'replaced', 'withdrawn']);

export function AssistantPermissionDisclosure({ state, tools, children }: {
    state: string;
    tools: string[];
    children: ReactNode;
}) {
    // Remount disclosure state on status changes, including remote decisions.
    return <PermissionDisclosure key={state} state={state} tools={tools}>{children}</PermissionDisclosure>;
}

function PermissionDisclosure({ state, tools, children }: { state: string; tools: string[]; children: ReactNode }) {
    const { t } = useTranslation();
    const [expanded, setExpanded] = useState(false);
    const completed = completedStates.has(state);
    const visible = !completed || expanded;
    const summary = <>
        <span className="min-w-0 flex-1 truncate text-sm" title={tools.join(', ')}>{tools.join(', ')}</span>
        <Badge variant={completed ? 'outline' : 'default'}>{t(`pages.deviceAssistant.permissionState.${state}`)}</Badge>
    </>;
    return <div className="space-y-3 rounded-md bg-muted/50 p-3">
        {completed ? <button type="button" className="flex w-full items-center gap-2 text-left"
            aria-expanded={visible} onClick={() => setExpanded((value) => !value)}>
            {visible ? <ChevronDown className="h-4 w-4 shrink-0" /> : <ChevronRight className="h-4 w-4 shrink-0" />}
            {summary}
            <span className="sr-only">{t('pages.deviceAssistant.permissionDetails')}</span>
        </button> : <div className="flex items-center gap-2">{summary}</div>}
        {visible && children}
    </div>;
}
