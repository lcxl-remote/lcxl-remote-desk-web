import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

type Session = { sessionId: string; conversationId: string | null; firstQuestion: string | null; updatedAt: string };

export function AssistantHistory({ deskId, disabled, onSelect }: {
    deskId: string;
    disabled: boolean;
    onSelect: (conversationId: string) => boolean;
}) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    const [sessions, setSessions] = useState<Session[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState(false);
    const [revision, setRevision] = useState(0);
    useEffect(() => {
        if (!open) return;
        const abort = new AbortController();
        setSessions([]);
        setLoading(true);
        setError(false);
        void (async () => {
            try {
                const response = await fetch(`/api/my/device-assistant-sessions?connection=${encodeURIComponent(deskId)}&limit=100`, {
                    credentials: 'include', headers: { Accept: 'application/json' }, signal: abort.signal,
                });
                const body = response.ok ? await response.json() : null;
                if (!Array.isArray(body?.data?.sessions)) throw new Error('Invalid session list');
                if (!abort.signal.aborted) setSessions(body.data.sessions);
            } catch {
                if (!abort.signal.aborted) setError(true);
            } finally {
                if (!abort.signal.aborted) setLoading(false);
            }
        })();
        return () => abort.abort();
    }, [open, deskId, revision]);
    return <>
        <Button type="button" variant="outline" size="sm" onClick={() => setOpen(true)}>
            {t('pages.deviceAssistant.history.title')}
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
                    {sessions.map((session) => <button key={session.sessionId} type="button"
                        disabled={disabled || !session.conversationId}
                        onClick={() => { if (session.conversationId && onSelect(session.conversationId)) setOpen(false); }}
                        className="block w-full rounded-lg border p-3 text-left hover:bg-muted disabled:opacity-50">
                        <span className="block whitespace-pre-wrap text-sm [overflow-wrap:anywhere]">{session.firstQuestion || t('pages.deviceAssistant.history.untitled')}</span>
                        <span className="mt-1 block text-xs text-muted-foreground">{session.updatedAt}</span>
                        {!session.conversationId && <span className="block text-xs">{t('pages.deviceAssistant.history.unavailable')}</span>}
                    </button>)}
                </div>
            </SheetContent>
        </Sheet>
    </>;
}
