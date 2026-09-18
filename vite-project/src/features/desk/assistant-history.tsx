import { formatLocalTime } from '@/lib/local-time';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { History, Loader2, Trash2 } from 'lucide-react';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { manageDeviceFileRecovery } from '@/services/clients';
import { AssistantBackupCleanup } from './assistant-backup-cleanup';

type Session = { sessionId: string; conversationId: string | null; firstQuestion: string | null; updatedAt: string; active?: boolean };

export function AssistantHistory({ deskId, deviceId, disabled, onSelect, onDeleted }: {
    deskId: string;
    deviceId?: string | null;
    disabled: boolean;
    onDeleted?: (conversationId: string | null) => void;
    onSelect: (conversationId: string) => boolean;
}) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    const [sessions, setSessions] = useState<Session[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState(false);
    const [deleting, setDeleting] = useState<Session | null>(null);
    const [busy, setBusy] = useState(false);
    const [deleteError, setDeleteError] = useState(false);
    const [revision, setRevision] = useState(0);
    const [backupSummary, setBackupSummary] = useState<'unknown' | 'present' | 'empty'>('unknown');
    const [backupCount, setBackupCount] = useState(0);
    const [cleanupPending, setCleanupPending] = useState(false);
    useEffect(() => {
        setBackupSummary('unknown');
        if (!deleting?.sessionId) return;
        const abort = new AbortController();
        void manageDeviceFileRecovery({ connection: deskId, device_id: deviceId,
            request: { command: { operation: 'query', conversation_id: deleting.sessionId } } },
        { signal: abort.signal }).then(response => {
            if (abort.signal.aborted || response.data?.outcome.kind !== 'page') return;
            setBackupCount(response.data.outcome.page.records.length);
            setBackupSummary(response.data.outcome.page.records.length ? 'present' : 'empty');
        }).catch(() => { /* Unknown remains explicit when the device is unavailable. */ });
        return () => abort.abort();
    }, [deleting?.sessionId, deskId, deviceId]);
    useEffect(() => {
        if (!open) return;
        const abort = new AbortController();
        setLoading(true);
        setError(false);
        const refresh = async () => {
            try {
                const response = await fetch(`/api/my/ai-assistant-sessions?connection=${encodeURIComponent(deskId)}&limit=100`, {
                    credentials: 'include', headers: { Accept: 'application/json' }, signal: abort.signal,
                });
                const body = response.ok ? await response.json() : null;
                if (!Array.isArray(body?.data?.sessions)) throw new Error('Invalid session list');
                if (!abort.signal.aborted) setSessions([...body.data.sessions].sort((left: Session, right: Session) =>
                    Number(Boolean(right.active)) - Number(Boolean(left.active))
                    || (Date.parse(right.updatedAt) || 0) - (Date.parse(left.updatedAt) || 0)
                    || left.sessionId.localeCompare(right.sessionId)));
            } catch {
                if (!abort.signal.aborted) setError(true);
            } finally {
                if (!abort.signal.aborted) setLoading(false);
            }
        };
        void refresh();
        const timer = window.setInterval(refresh, 3000);
        return () => { abort.abort(); window.clearInterval(timer); };
    }, [open, deskId, revision]);
    return <>
        <Button type="button" variant="outline" size="sm" className="assistant-action" aria-label={t('pages.aiAssistant.history.title')} title={t('pages.aiAssistant.history.title')} onClick={() => setOpen(true)}>
            <History className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('pages.aiAssistant.history.title')}</span>
        </Button>
        <Sheet open={open} onOpenChange={setOpen}>
            <SheetContent className="flex flex-col sm:max-w-md">
                <SheetHeader>
                    <SheetTitle>{t('pages.aiAssistant.history.title')}</SheetTitle>
                    <SheetDescription>{t('pages.aiAssistant.history.hint')}</SheetDescription>
                </SheetHeader>
                {disabled && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.history.busy')}</p>}
                {cleanupPending && <p role="status" className="text-sm text-muted-foreground">{t('pages.fileRecovery.deletionPending')}</p>}
                <div className="min-h-0 flex-1 space-y-2 overflow-y-auto py-4">
                    <AssistantBackupCleanup />
                    {loading && <p role="status">{t('pages.aiAssistant.history.loading')}</p>}
                    {error && <div role="alert">
                        <p>{t('pages.aiAssistant.history.error')}</p>
                        <Button variant="outline" onClick={() => setRevision((value) => value + 1)}>{t('pages.aiAssistant.history.retry')}</Button>
                    </div>}
                    {!loading && !error && sessions.length === 0 && <p>{t('pages.aiAssistant.history.empty')}</p>}
                    {sessions.map((session) => <div key={session.sessionId} className="flex items-start gap-2 rounded-lg border p-3">
                        <Button variant="unstyled" type="button" disabled={disabled || busy || !session.conversationId}
                            onClick={() => { if (session.conversationId && onSelect(session.conversationId)) setOpen(false); }}
                            className="min-w-0 flex-1 text-left hover:text-primary disabled:opacity-50">
                            <span className="block whitespace-pre-wrap text-sm [overflow-wrap:anywhere]">{session.firstQuestion || t('pages.aiAssistant.history.untitled')}</span>
                            <span className="mt-1 block text-xs text-muted-foreground">{formatLocalTime(session.updatedAt)}</span>
                            {!session.conversationId && <span className="block text-xs">{t('pages.aiAssistant.history.unavailable')}</span>}
                        </Button>
                        {session.active && <Loader2 className="mt-2 size-4 shrink-0 animate-spin" role="status" aria-label={t('pages.aiAssistant.history.running')} />}
                        <Button type="button" variant="ghost" size="icon" disabled={disabled || busy}
                            aria-label={t('pages.aiAssistant.history.delete')}
                            onClick={() => { setDeleting(session); setDeleteError(false); }}><Trash2 className="size-4" /></Button>
                    </div>)}
                </div>
            </SheetContent>
        </Sheet>
        <Dialog open={!!deleting} onOpenChange={value => { if (!value && !busy) setDeleting(null); }}>
            <DialogContent>
                <DialogHeader><DialogTitle>{t('pages.aiAssistant.history.deleteTitle')}</DialogTitle>
                    <DialogDescription>{t('pages.aiAssistant.history.deleteHint')}</DialogDescription></DialogHeader>
                <p className="break-words text-sm">{deleting?.firstQuestion || t('pages.aiAssistant.history.untitled')}</p>
                <p className="text-sm text-amber-700 dark:text-amber-300">{t(`pages.fileRecovery.deleteBackup.${backupSummary}`, { count: backupCount })}</p>
                {(deleting?.active || sessions.some(value => value.sessionId === deleting?.sessionId && value.active)) && <p className="text-sm text-destructive">{t('pages.aiAssistant.history.deleteRunning')}</p>}
                {deleteError && <p role="alert" className="text-sm text-destructive">{t('pages.aiAssistant.history.deleteError')}</p>}
                <DialogFooter>
                    <Button variant="outline" disabled={busy} onClick={() => setDeleting(null)}>{t('pages.aiAssistant.history.keep')}</Button>
                    <Button variant="destructive" disabled={busy || !deleting} onClick={() => void (async () => {
                        if (!deleting || busy) return;
                        const selected = deleting;
                        setBusy(true); setDeleteError(false);
                        try {
                            const response = await fetch('/api/my/ai-assistant-session/delete', { method: 'POST', credentials: 'include',
                                headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ connection: deskId, session: selected.sessionId }) });
                            const body = response.ok ? await response.json() : null;
                            if (body?.data?.deleted !== true) throw new Error('Deletion failed');
                            setCleanupPending(body.data.backup_cleanup_pending === true);
                            setSessions(current => current.filter(value => value.sessionId !== selected.sessionId));
                            onDeleted?.(selected.conversationId); setDeleting(null); setRevision(value => value + 1);
                        } catch { setDeleteError(true); } finally { setBusy(false); }
                    })()}>{busy && <Loader2 className="mr-2 size-4 animate-spin" />}{t('pages.aiAssistant.history.delete')}</Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    </>;
}
