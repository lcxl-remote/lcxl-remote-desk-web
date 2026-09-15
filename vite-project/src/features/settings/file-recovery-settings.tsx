import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Archive, Download, Loader2, RefreshCw, Trash2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Disclosure } from '@/components/ui/disclosure';
import { Input } from '@/components/ui/input';
import { RecoveryFailure, recoveryErrorKey, requireRecoveryZip } from '@/lib/file-recovery-error';
import { formatLocalTime } from '@/lib/local-time';
import { queryLocalFileRecovery, updateLocalFileRecoveryPolicy, retryLocalFileRecoveryCleanup, exportLocalFileRecovery, discardLocalFileRecovery, confirmLocalFileRecoveryClock, manageDeviceFileRecovery, exportDeviceFileRecovery } from '@/services/clients';
import type { FileRecoveryPageDto, FileRecoveryCommand } from '@/services/types';
import { AssistantBackupCleanup } from '@/features/desk/assistant-backup-cleanup';

type RecoverySettingsProps = { target?: { connection: string; device_id?: string | null } };

export function FileRecoverySettings({ target }: RecoverySettingsProps) {
    // A new destination must not inherit another device's page, authority,
    // OS account, cursor or pending confirmation.
    const key = target ? JSON.stringify([target.connection, target.device_id ?? null]) : 'local';
    return <ScopedFileRecoverySettings key={key} target={target} />;
}

function ScopedFileRecoverySettings({ target }: RecoverySettingsProps) {
    const { t } = useTranslation();
    const active = useRef(true);
    const running = useRef(false);
    useEffect(() => {
        active.current = true;
        return () => { active.current = false; };
    }, []);
    const current = async <T,>(request: () => Promise<T>): Promise<T> => {
        if (!active.current) throw new Error('Recovery destination changed');
        const result = await request();
        if (!active.current) throw new Error('Recovery destination changed');
        return result;
    };
    const [page, setPage] = useState<FileRecoveryPageDto | null>(null);
    const [days, setDays] = useState('7');
    const [capacity, setCapacity] = useState('100');
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState<string | null>(null);
    const [confirm, setConfirm] = useState(false);
    const [discardId, setDiscardId] = useState<string | null>(null);
    const [clockTime, setClockTime] = useState<number | null>(null);
    const [authority, setAuthority] = useState<string | undefined>();
    const [osUser, setOsUser] = useState<string | undefined>();
    // Pin the oldest pending item even when it is outside the current cursor page.
    const records = page ? (page.oldest_pending_record
        ? [page.oldest_pending_record, ...page.records.filter(record => record.recovery_id !== page.oldest_pending_record?.recovery_id)]
        : page.records) : [];
    const local = ['localhost', '127.0.0.1', '[::1]'].includes(window.location.hostname);
    const available = !!target || local;
    const remote = async (command: FileRecoveryCommand) => {
        if (!target) throw new Error('Missing device');
        const response = await current(() => manageDeviceFileRecovery({ ...target, request: { command, expected_authority: authority, expected_os_user: osUser } }));
        if (!response.data) throw new Error('Device backup storage unavailable');
        if (response.data.outcome.kind === 'unavailable') throw new RecoveryFailure(response.data.outcome.reason);
        setAuthority(response.data.authority);
        setOsUser(response.data.os_user);
        return response.data.outcome;
    };
    const valid = Number.isInteger(Number(days)) && Number(days) >= 1 && Number(days) <= 3650
        && Number.isInteger(Number(capacity)) && Number(capacity) >= 1 && Number(capacity) <= 10240;
    const load = async (more = false) => {
        const after = more ? page?.next_cursor : undefined;
        const outcome = target ? await remote({ operation: 'query', after }) : null;
        const next = target ? (outcome?.kind === 'page' ? outcome.page : null)
            : (await current(() => queryLocalFileRecovery({ after }))).data;
        if (!next) throw new Error('Missing recovery page');
        setPage(more && page ? { ...next, records: [...page.records, ...next.records] } : next);
        if (!more) {
            setDays(String(next.policy.retention_days));
            setCapacity(String(next.policy.max_bytes / 1048576));
        }
    };
    const run = async (action: () => Promise<void>) => {
        if (!active.current || running.current) return;
        running.current = true;
        setBusy(true); setStatus(null);
        try { await action(); } catch (error) {
            if (active.current) setStatus(recoveryErrorKey(error));
        } finally {
            running.current = false;
            if (active.current) setBusy(false);
        }
    };
    const save = () => run(async () => {
        if (!valid) return;
        const policy = { retention_days: Number(days), max_bytes: Number(capacity) * 1048576 };
        if (target) {
            if ((await remote({ operation: 'set_policy', ...policy })).kind !== 'policy') throw new Error('Missing recovery policy');
        } else if (!(await current(() => updateLocalFileRecoveryPolicy(policy))).data) throw new Error('Missing recovery policy');
        await load(); setStatus('saved');
    });
    const cleanup = () => run(async () => {
        const outcome = target ? await remote({ operation: 'retry_cleanup' }) : null;
        const report = target ? (outcome?.kind === 'cleanup' ? outcome.report : null)
            : (await current(() => retryLocalFileRecoveryCleanup())).data;
        if (!report) throw new Error('Missing cleanup result');
        await load();
        setStatus(report.pending_files || report.unknown_outcomes ? 'pending' : 'cleaned');
    });
    const download = (id: string) => run(async () => {
        const record = records.find(row => row.recovery_id === id);
        if (!record) throw new Error('Missing backup');
        const data = await current(() => target ? exportDeviceFileRecovery({ ...target, recovery_id: id,
            conversation_id: record.conversation_id, expected_authority: authority, expected_os_user: osUser }, { responseType: 'blob' })
            : exportLocalFileRecovery({ recovery_id: id }, { responseType: 'blob' }));
        const zip = await current(() => requireRecoveryZip(data));
        const url = URL.createObjectURL(zip);
        const anchor = document.createElement('a');
        anchor.href = url; anchor.download = 'file-recovery.zip'; anchor.click();
        window.setTimeout(() => URL.revokeObjectURL(url), 60000);
    });
    const discard = (id: string) => run(async () => {
        const record = records.find(row => row.recovery_id === id);
        if (!record) throw new Error('Missing backup');
        const body = { recovery_id: id, conversation_id: record.conversation_id, confirmed: true };
        const outcome = target ? await remote({ operation: 'discard', ...body }) : null;
        const report = target ? (outcome?.kind === 'cleanup' ? outcome.report : null)
            : (await current(() => discardLocalFileRecovery(body))).data;
        if (!report) throw new Error('Missing discard result');
        try { await load(); } catch { if (active.current) setStatus('discardRefreshFailed'); return; }
        setStatus(report.pending_files ? 'pending' : 'discarded');
    });
    const acceptClock = (displayedTime: number) => run(async () => {
        const body = { displayed_time_unix_ms: displayedTime, confirmed: true };
        const outcome = target ? await remote({ operation: 'confirm_clock', ...body }) : null;
        const report = target ? (outcome?.kind === 'cleanup' ? outcome.report : null)
            : (await current(() => confirmLocalFileRecoveryClock(body))).data;
        if (!report) throw new Error('Missing clock confirmation');
        try { await load(); } catch { if (active.current) setStatus('clockRefreshFailed'); return; }
        setStatus('clockConfirmed');
    });
    return <Card>
        <CardHeader><CardTitle className="flex items-center gap-2"><Archive className="size-5" />{t('pages.fileRecovery.title')}</CardTitle>
            <CardDescription>{t('pages.fileRecovery.description')}</CardDescription></CardHeader>
        <CardContent><Disclosure title={t('pages.fileRecovery.manage')}>
            <div className="mt-3 space-y-4">
                <Alert className="border-amber-500/50 bg-amber-500/10"><AlertDescription>{t(target ? 'pages.fileRecovery.remoteOwner' : 'pages.fileRecovery.localOnly')}</AlertDescription></Alert>
                <div className="flex flex-wrap gap-2">
                    <Button variant="outline" disabled={!available || busy} onClick={() => void run(() => load())}>
                        {busy ? <Loader2 className="size-4 animate-spin" /> : <RefreshCw className="size-4" />}{t('pages.fileRecovery.refresh')}</Button>
                    <Button variant="outline" disabled={!available || busy || !page} onClick={() => void cleanup()}>{t('pages.fileRecovery.retry')}</Button>
                </div>
                {page && <>
                    {page.cleanup_warning && <Alert variant="destructive"><AlertDescription>{t(`pages.fileRecovery.error.${page.cleanup_warning}`)}</AlertDescription></Alert>}
                    {page.clock_confirmation_time_unix_ms != null && <Button variant="outline" disabled={busy} onClick={() => setClockTime(page.clock_confirmation_time_unix_ms ?? null)}>{t('pages.fileRecovery.confirmClock')}</Button>}
                    {page.oldest_pending_at_unix_ms != null && <p className="text-sm text-amber-700 dark:text-amber-300">{t('pages.fileRecovery.oldestPending', { time: formatLocalTime(new Date(page.oldest_pending_at_unix_ms).toISOString()) })}</p>}
                    <p className="text-sm">{t('pages.fileRecovery.usage', { used: (page.used_bytes / 1048576).toFixed(2), limit: (page.policy.max_bytes / 1048576).toFixed(0) })}</p>
                    <p className="text-sm text-muted-foreground">{t('pages.fileRecovery.reserved', { reserved: (page.reserved_bytes / 1048576).toFixed(2) })}</p>
                    <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); if (!valid || busy) return; if (Number(days) < page.policy.retention_days) setConfirm(true); else void save(); }}>
                        <div className="grid gap-3 sm:grid-cols-2">
                            <label className="space-y-1 text-sm">{t('pages.fileRecovery.days')}<Input type="number" min={1} max={3650} step={1} value={days} disabled={busy} onChange={(e) => setDays(e.target.value)} /></label>
                            <label className="space-y-1 text-sm">{t('pages.fileRecovery.capacity')}<Input type="number" min={1} max={10240} step={1} value={capacity} disabled={busy} onChange={(e) => setCapacity(e.target.value)} /></label>
                        </div>
                        <p className="text-xs text-muted-foreground">{t('pages.fileRecovery.policyHint')}</p>
                        <Button type="submit" disabled={busy || !valid}>{t('pages.fileRecovery.save')}</Button>
                    </form>
                    <div className="space-y-2">
                        <p className="text-sm text-muted-foreground">{t(page.next_cursor ? 'pages.fileRecovery.loadedMore' : 'pages.fileRecovery.loaded', { count: records.length })}</p>
                        {records.length === 0 && <p className="text-sm text-muted-foreground">{t('pages.fileRecovery.empty')}</p>}
                        {records.map((record) => <div key={record.recovery_id} className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-3">
                            <div className="min-w-0">{record.recovery_id === page.oldest_pending_record?.recovery_id && <p className="text-xs text-amber-700 dark:text-amber-300">{t('pages.fileRecovery.oldestPending', { time: formatLocalTime(new Date(record.created_at_unix_ms).toISOString()) })}</p>}<p className="break-all text-sm font-medium">{record.file_name}</p>
                                <p className="text-xs text-muted-foreground">{record.change_state === 'outcome_unknown'
                                    ? t('pages.fileRecovery.unknownRetained')
                                    : t('pages.fileRecovery.until', { time: formatLocalTime(new Date(record.expires_at_unix_ms).toISOString()) })}</p>
                                {(record.cleanup_pending || record.cleanup_reason) && <p className="text-xs text-amber-700 dark:text-amber-300">{t(['temporary_file_pending', 'storage_unavailable', 'quota_pending', 'quota_settlement_pending'].includes(record.cleanup_reason ?? '') ? `pages.fileRecovery.error.${record.cleanup_reason}` : 'pages.fileRecovery.pending')}</p>}
                            </div>
                            <div className="flex flex-wrap gap-2"><Button size="sm" variant="outline" disabled={busy || !record.export_available} onClick={() => void download(record.recovery_id)}><Download className="size-4" />{t('pages.fileRecovery.export')}</Button>
                                <Button size="sm" variant="outline" disabled={busy || !['succeeded', 'aborted', 'outcome_unknown'].includes(record.change_state)} onClick={() => setDiscardId(record.recovery_id)}><Trash2 className="size-4" />{t('pages.fileRecovery.discard')}</Button></div>
                        </div>)}
                        {page.next_cursor && <Button variant="outline" disabled={busy} onClick={() => void run(() => load(true))}>{t('pages.fileRecovery.more')}</Button>}
                    </div>
                </>}
                {status && <p role="status" className="text-sm">{t(`pages.fileRecovery.${status}`)}</p>}
            </div>
        </Disclosure><div className="mt-4"><AssistantBackupCleanup /></div></CardContent>
        <AlertDialog open={confirm} onOpenChange={setConfirm}><AlertDialogContent>
            <AlertDialogHeader><AlertDialogTitle>{t('pages.fileRecovery.shortenTitle')}</AlertDialogTitle><AlertDialogDescription>{t('pages.fileRecovery.shortenBody')}</AlertDialogDescription></AlertDialogHeader>
            <AlertDialogFooter><AlertDialogCancel>{t('pages.fileRecovery.cancel')}</AlertDialogCancel><AlertDialogAction onClick={() => void save()}>{t('pages.fileRecovery.save')}</AlertDialogAction></AlertDialogFooter>
        </AlertDialogContent></AlertDialog>
        <AlertDialog open={discardId !== null} onOpenChange={(open) => { if (!open) setDiscardId(null); }}><AlertDialogContent>
            <AlertDialogHeader><AlertDialogTitle>{t('pages.fileRecovery.discardTitle')}</AlertDialogTitle><AlertDialogDescription>{t('pages.fileRecovery.discardBody', { name: records.find(record => record.recovery_id === discardId)?.file_name })}</AlertDialogDescription></AlertDialogHeader>
            <AlertDialogFooter><AlertDialogCancel>{t('pages.fileRecovery.cancel')}</AlertDialogCancel><AlertDialogAction onClick={() => { if (discardId) void discard(discardId); }}>{t('pages.fileRecovery.discard')}</AlertDialogAction></AlertDialogFooter>
        </AlertDialogContent></AlertDialog>
        <AlertDialog open={clockTime !== null} onOpenChange={(open) => { if (!open) setClockTime(null); }}><AlertDialogContent>
            <AlertDialogHeader><AlertDialogTitle>{t('pages.fileRecovery.confirmClock')}</AlertDialogTitle><AlertDialogDescription>{t('pages.fileRecovery.clockBody', { time: clockTime == null ? '' : formatLocalTime(new Date(clockTime).toISOString()) })}</AlertDialogDescription></AlertDialogHeader>
            <AlertDialogFooter><AlertDialogCancel>{t('pages.fileRecovery.cancel')}</AlertDialogCancel><AlertDialogAction onClick={() => { if (clockTime !== null) void acceptClock(clockTime); }}>{t('pages.fileRecovery.confirmClock')}</AlertDialogAction></AlertDialogFooter>
        </AlertDialogContent></AlertDialog>
    </Card>;
}
