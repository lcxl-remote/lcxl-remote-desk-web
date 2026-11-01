import { assistantSourceHref } from '@/components/markdown-source-links';
import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { AlertDialog, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter,
    AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog';
import type { AssistantHistoryConversations } from '@/services/types';
import { assistantDataErrorKey, deleteRetainedAssistantConversation, listRetainedAssistantConversations } from './assistant-data-api';

export function AssistantRetainedConversations() {
    const { t } = useTranslation();
    const [page, setPage] = useState<AssistantHistoryConversations | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const [target, setTarget] = useState<string | null>(null);
    const [sending, setSending] = useState(false);
    const epoch = useRef(0), mutating = useRef(false);
    const load = async (previous?: AssistantHistoryConversations) => {
        const stamp = ++epoch.current; setLoading(true); setError('');
        try {
            const next = await listRetainedAssistantConversations(previous?.next_cursor ?? undefined);
            if (stamp === epoch.current) setPage(previous ? { ...next, conversations: [...previous.conversations,
                ...next.conversations.filter(item => !previous.conversations.some(old => old.conversation_id === item.conversation_id))],
                partial: next.partial || previous.partial } : next);
        } catch (cause) { if (stamp === epoch.current) { setPage(null); setError(assistantDataErrorKey(cause)); } }
        finally { if (stamp === epoch.current) setLoading(false); }
    };
    useEffect(() => { void load(); return () => { epoch.current++; }; }, []);
    const remove = async () => {
        if (!target || mutating.current) return;
        mutating.current = true; setSending(true); setError('');
        try { await deleteRetainedAssistantConversation(target); setTarget(null); await load(); }
        catch (cause) { setError(assistantDataErrorKey(cause)); }
        finally { mutating.current = false; setSending(false); }
    };
    return <div className="space-y-3">
        <div className="flex items-center justify-between gap-3"><h2 className="text-lg font-medium">{t('pages.aiAssistant.historySearch.conversations')}</h2>
            <Button variant="outline" disabled={loading || sending} onClick={() => void load()}>{t('pages.aiAssistant.memory.refresh')}</Button></div>
        {loading && <p role="status">{t('pages.aiAssistant.memory.loading')}</p>}
        {error && <p role="alert" className="text-destructive">{t(error)}</p>}
        {page?.partial && <p>{t('pages.aiAssistant.historySearch.partial')}</p>}
        {!loading && page?.conversations.length === 0 && <p>{t('pages.aiAssistant.historySearch.empty')}</p>}
        {page?.conversations.map(item => <Card key={item.conversation_id}><CardContent className="space-y-2 pt-4">
            <p className="text-xs text-muted-foreground">{item.source.source_device?.current_name ?? item.source.source_device?.recorded_name}
                {' · '}{t(item.execution_available ? 'pages.aiAssistant.historySearch.executionAvailable' : 'pages.aiAssistant.historySearch.readonly')}</p>
            <p className="line-clamp-3 whitespace-pre-wrap text-sm">{item.source.content}</p>
            <div className="flex gap-2"><Button variant="link" asChild><Link to={assistantSourceHref(item.source.source_id, item.source.revision)}>
                {t('pages.aiAssistant.memory.openOriginal')}</Link></Button>
                <Button variant="ghost" disabled={loading || sending} onClick={() => setTarget(item.conversation_id)}>{t('pages.aiAssistant.historySearch.delete')}</Button></div>
        </CardContent></Card>)}
        {page?.next_cursor && <Button variant="outline" disabled={loading || sending} onClick={() => void load(page)}>{t('pages.aiAssistant.memory.more')}</Button>}
        <AlertDialog open={target !== null} onOpenChange={open => { if (!open && !mutating.current) setTarget(null); }}>
            <AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{t('pages.aiAssistant.historySearch.delete')}</AlertDialogTitle>
                <AlertDialogDescription>{t('pages.aiAssistant.historySearch.deleteDescription')}</AlertDialogDescription></AlertDialogHeader>
                <AlertDialogFooter><AlertDialogCancel disabled={sending}>{t('pages.aiAssistant.memory.cancel')}</AlertDialogCancel>
                    <Button variant="destructive" disabled={sending} onClick={() => void remove()}>{t('pages.aiAssistant.historySearch.delete')}</Button></AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
    </div>;
}
