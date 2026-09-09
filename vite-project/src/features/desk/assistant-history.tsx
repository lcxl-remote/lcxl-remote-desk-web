import { formatLocalTime } from '@/lib/local-time';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { History, Loader2, Trash2 } from 'lucide-react';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter } from '@/components/ui/dialog';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

type Session = { sessionId: string; conversationId: string | null; firstQuestion: string | null; updatedAt: string; active?: boolean };

export function AssistantHistory({ deskId, disabled, onSelect, onDeleted }: {
    deskId: string;
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
    useEffect(() => {
        if (!open) return;
        const abort = new AbortController();
        setLoading(true);
        setError(false);
        const refresh = async () => {
            try {
                const response = await fetch(`/api/my/device-assistant-sessions?connection=${encodeURIComponent(deskId)}&limit=100`, {
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
        <Button type="button" variant="outline" size="sm" className="assistant-action" aria-label={t('pages.deviceAssistant.history.title')} title={t('pages.deviceAssistant.history.title')} onClick={() => setOpen(true)}>
            <History className="h-4 w-4 shrink-0" aria-hidden="true" /><span className="assistant-action-label">{t('pages.deviceAssistant.history.title')}</span>
        </Button>
        <Sheet open={open} onOpenChange={setOpen}>
            <SheetContent className="flex flex-col sm:max-w-md">
                <SheetHeader>
                    <SheetTitle>{t('pages.deviceAssistant.history.title')}</SheetTitle>
                    <SheetDescription>{t('pages.deviceAssistant.history.hint')}</SheetDescription>
                </SheetHeader>
                {disabled && <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.history.busy')}</p>}
                <div className="min-h-0 flex-1 space-y-2 overflow-y-auto py-4">
                    {loading && <p role="status">{t('pages.deviceAssistant.history.loading')}</p>}
                    {error && <div role="alert">
                        <p>{t('pages.deviceAssistant.history.error')}</p>
                        <Button variant="outline" onClick={() => setRevision((value) => value + 1)}>{t('pages.deviceAssistant.history.retry')}</Button>
                    </div>}
                    {!loading && !error && sessions.length === 0 && <p>{t('pages.deviceAssistant.history.empty')}</p>}
                    {sessions.map((session) => <div key={session.sessionId} className="flex items-start gap-2 rounded-lg border p-3">
                        <button type="button" disabled={disabled || busy || !session.conversationId}
                            onClick={() => { if (session.conversationId && onSelect(session.conversationId)) setOpen(false); }}
                            className="min-w-0 flex-1 text-left hover:text-primary disabled:opacity-50">
                            <span className="block whitespace-pre-wrap text-sm [overflow-wrap:anywhere]">{session.firstQuestion || t('pages.deviceAssistant.history.untitled')}</span>
                            <span className="mt-1 block text-xs text-muted-foreground">{formatLocalTime(session.updatedAt)}</span>
                            {!session.conversationId && <span className="block text-xs">{t('pages.deviceAssistant.history.unavailable')}</span>}
                        </button>
                        {session.active && <Loader2 className="mt-2 size-4 shrink-0 animate-spin" role="status" aria-label={t('pages.deviceAssistant.history.running')} />}
                        <Button type="button" variant="ghost" size="icon" disabled={disabled || busy}
                            aria-label={t('pages.deviceAssistant.history.delete')}
                            onClick={() => { setDeleting(session); setDeleteError(false); }}><Trash2 className="size-4" /></Button>
                    </div>)}
                </div>
            </SheetContent>
        </Sheet>
        <Dialog open={!!deleting} onOpenChange={value => { if (!value && !busy) setDeleting(null); }}>
            <DialogContent>
                <DialogHeader><DialogTitle>{t('pages.deviceAssistant.history.deleteTitle')}</DialogTitle>
                    <DialogDescription>{t('pages.deviceAssistant.history.deleteHint')}</DialogDescription></DialogHeader>
                <p className="break-words text-sm">{deleting?.firstQuestion || t('pages.deviceAssistant.history.untitled')}</p>
                {(deleting?.active || sessions.some(value => value.sessionId === deleting?.sessionId && value.active)) && <p className="text-sm text-destructive">{t('pages.deviceAssistant.history.deleteRunning')}</p>}
                {deleteError && <p role="alert" className="text-sm text-destructive">{t('pages.deviceAssistant.history.deleteError')}</p>}
                <DialogFooter>
                    <Button variant="outline" disabled={busy} onClick={() => setDeleting(null)}>{t('pages.deviceAssistant.history.keep')}</Button>
                    <Button variant="destructive" disabled={busy || !deleting} onClick={() => void (async () => {
                        if (!deleting || busy) return;
                        const selected = deleting;
                        setBusy(true); setDeleteError(false);
                        try {
                            const response = await fetch('/api/my/device-assistant-session/delete', { method: 'POST', credentials: 'include',
                                headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ connection: deskId, session: selected.sessionId }) });
                            const body = response.ok ? await response.json() : null;
                            if (body?.data?.deleted !== true) throw new Error('Deletion failed');
                            setSessions(current => current.filter(value => value.sessionId !== selected.sessionId));
                            onDeleted?.(selected.conversationId); setDeleting(null); setRevision(value => value + 1);
                        } catch { setDeleteError(true); } finally { setBusy(false); }
                    })()}>{busy && <Loader2 className="mr-2 size-4 animate-spin" />}{t('pages.deviceAssistant.history.delete')}</Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    </>;
}
