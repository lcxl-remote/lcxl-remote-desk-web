import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Disclosure } from '@/components/ui/disclosure';
import { formatLocalTime } from '@/lib/local-time';
import { listFileRecoveryCleanup, retryFileRecoveryCleanup } from '@/services/clients';
import type { FileRecoveryCleanupPage } from '@/services/types';

/** Central status remains readable even when the device or conversation is gone. */
export function AssistantBackupCleanup() {
    const { t } = useTranslation();
    const [page, setPage] = useState<FileRecoveryCleanupPage | null>(null);
    const [busy, setBusy] = useState(false);
    const [failed, setFailed] = useState(false);
    const [retryStatus, setRetryStatus] = useState<'scheduled' | 'unchanged' | null>(null);
    const retry = async (conversation: string) => {
        if (busy) return;
        setBusy(true); setFailed(false); setRetryStatus(null);
        try {
            const result = await retryFileRecoveryCleanup({ conversation_id: conversation });
            if (!result.data) throw new Error('Retry request not confirmed');
            setRetryStatus(result.data.scheduled ? 'scheduled' : 'unchanged');
        } catch { setFailed(true); }
        finally { setBusy(false); }
    };
    const load = async (more = false) => {
        if (busy) return;
        setBusy(true); setFailed(false);
        try {
            const result = await listFileRecoveryCleanup({ after: more ? page?.next_cursor ?? undefined : undefined });
            if (!result.data) throw new Error('Cleanup status unavailable');
            const next = result.data;
            setPage(previous => more && previous ? { ...next, records: [...previous.records, ...next.records] } : next);
        } catch { setFailed(true); }
        finally { setBusy(false); }
    };
    return <Disclosure title={t('pages.fileRecovery.cleanupStatus.title')}>
        <div className="mt-3 space-y-3">
            <p className="text-xs text-muted-foreground">{t('pages.fileRecovery.cleanupStatus.hint')}</p>
            <Button type="button" size="sm" variant="outline" aria-label={t('pages.fileRecovery.cleanupStatus.refresh')} disabled={busy} onClick={() => void load()}>
                {busy ? <Loader2 className="size-4 animate-spin" /> : <RefreshCw className="size-4" />}{t('pages.fileRecovery.refresh')}
            </Button>
            {failed && <p role="alert" className="text-sm text-destructive">{t('pages.fileRecovery.cleanupStatus.failed')}</p>}
            {retryStatus && <p role="status" className="text-sm">{t(`pages.fileRecovery.cleanupStatus.${retryStatus}`)}</p>}
            {page?.records.map(row => <div key={row.conversation_id} className="space-y-1 rounded-md border p-3 text-sm">
                <p>{t('pages.fileRecovery.cleanupStatus.deletedAt', { time: formatLocalTime(new Date(row.created_at_unix_ms).toISOString()) })}</p>
                <p>{t(`pages.fileRecovery.cleanupStatus.${row.reason}`)}</p>
                <p className="text-xs text-muted-foreground">{t('pages.fileRecovery.cleanupStatus.attempts', { count: row.attempts })}</p>
                <Button type="button" size="sm" variant="outline" disabled={busy} onClick={() => void retry(row.conversation_id)}>{t('pages.fileRecovery.retry')}</Button>
            </div>)}
            {page && !page.records.length && !page.next_cursor && <p className="text-sm">{t('pages.fileRecovery.cleanupStatus.empty')}</p>}
            {page?.next_cursor && <Button type="button" variant="outline" disabled={busy} onClick={() => void load(true)}>{t('pages.fileRecovery.more')}</Button>}
        </div>
    </Disclosure>;
}
