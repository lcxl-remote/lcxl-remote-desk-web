import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { LoaderCircle } from 'lucide-react';
import type { BackgroundTaskDto, CommandTaskDto } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import type { DeviceAssistantToolActivity } from './use-device-assistant-chat';
import { formatLocalTime } from '@/lib/local-time';

export function AssistantBackgroundTasks({ open, onOpenChange, commands, providers, tools,
    connected, canCancelProvider, cancelling, onCancel }: {
    open: boolean;
    onOpenChange: (open: boolean) => void;
    commands: CommandTaskDto[];
    providers: BackgroundTaskDto[];
    tools: DeviceAssistantToolActivity[];
    connected: boolean;
    canCancelProvider: boolean;
    cancelling: string | null;
    onCancel: (kind: 'command' | 'provider', id: string) => Promise<void>;
}) {
    const { t } = useTranslation();
    const [notice, setNotice] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const tasks = [
        ...commands.map(task => ({ kind: 'command' as const, id: task.taskId, name: t('pages.deviceAssistant.tasks.command'),
            state: task.state, updatedAt: task.updatedAt, reference: task.taskId,
            result: task.result, truncated: task.resultTruncated, supportsCancel: connected })),
        ...providers.map(task => ({ kind: 'provider' as const, id: task.taskId, name: task.toolName,
            state: task.state, updatedAt: task.updatedAt, reference: task.taskId,
            result: tools.find(tool => tool.callId === task.callId)?.output ?? null,
            truncated: false, supportsCancel: canCancelProvider && task.supportsCancel })),
    ].sort((a, b) => {
        const active = (state: string) => ['running', 'cancel_requested', 'outcome_unknown'].includes(state) ? 1 : 0;
        return active(b.state) - active(a.state) || b.updatedAt.localeCompare(a.updatedAt);
    });
    const cancel = async (kind: 'command' | 'provider', id: string) => {
        setError(null); setNotice(null);
        try {
            await onCancel(kind, id);
            setNotice(t('pages.deviceAssistant.tasks.cancelSent'));
        } catch {
            setError(t('pages.deviceAssistant.tasks.cancelFailed'));
        }
    };
    return <Sheet open={open} onOpenChange={onOpenChange}>
        <SheetContent className="flex w-full flex-col overflow-hidden sm:max-w-xl">
            <SheetHeader>
                <SheetTitle>{t('pages.deviceAssistant.tasks.title')}</SheetTitle>
                <SheetDescription>{t('pages.deviceAssistant.tasks.description')}</SheetDescription>
            </SheetHeader>
            <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-4">
                {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
                {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
                {!connected && <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.tasks.offline')}</p>}
                {tasks.length === 0 && <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.tasks.empty')}</p>}
                {tasks.map(task => <section key={`${task.kind}:${task.id}`} className="space-y-2 rounded-md border p-3">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                        <p className="text-sm font-medium">{task.name}</p>
                        <Badge variant="outline">{t(`pages.deviceAssistant.backgroundState.${task.state}`)}</Badge>
                    </div>
                    <p className="break-all text-xs text-muted-foreground">{task.reference}</p>
                    <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.backgroundUpdated', { time: formatLocalTime(task.updatedAt) })}</p>
                    {task.result != null ? <details>
                        <summary className="cursor-pointer text-sm">{t('pages.deviceAssistant.tasks.result')}</summary>
                        <pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2 text-xs">{task.result}</pre>
                        {task.truncated && <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.tasks.truncated')}</p>}
                    </details> : <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.tasks.noResult')}</p>}
                    {task.supportsCancel && ['running', 'outcome_unknown'].includes(task.state) && <Button type="button" size="sm" variant="outline"
                        disabled={cancelling !== null} onClick={() => void cancel(task.kind, task.id)}>
                        {cancelling === `${task.kind}:${task.id}` && <LoaderCircle className="mr-1 h-4 w-4 animate-spin" aria-hidden="true" />}
                        {t('pages.deviceAssistant.tasks.cancel')}
                    </Button>}
                </section>)}
            </div>
        </SheetContent>
    </Sheet>;
}
