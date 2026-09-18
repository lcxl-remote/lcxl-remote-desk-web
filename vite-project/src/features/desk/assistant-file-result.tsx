import { recoveryErrorKey } from '@/lib/file-recovery-error';
import { formatLocalTime } from '@/lib/local-time';
import { Disclosure } from '@/components/ui/disclosure';
import { useTranslation } from 'react-i18next';
import { useRef, useState } from 'react';
import { Download, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';

type FileReceipt = { operation: 'create' | 'update' | 'delete'; verified: boolean; fileName?: string; bytes?: number; digest?: string; referenceUnavailable?: boolean; recovery?: { id: string; expiresAt: number; cleanupPending: boolean } };
const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
const digest = (value: unknown): value is string => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);

export function parseFileReceipt(text: string): FileReceipt | null {
    try {
        const completion: unknown = JSON.parse(text);
        if (!record(completion) || !record(completion.output) || !record(completion.output.value)) return null;
        const value = completion.output.value;
        if (completion.output.kind === 'batch_document_artifact' && completion.result === 'verified'
            && typeof value.file_name === 'string' && value.file_name.length > 0
            && digest(value.sha256) && digest(value.validation_sha256)
            && Number.isSafeInteger(value.byte_len) && (value.byte_len as number) > 0
            && Number.isSafeInteger(value.validation_byte_len) && (value.validation_byte_len as number) > 0) {
            return { operation: 'create', verified: true, fileName: value.file_name, bytes: value.byte_len as number, digest: value.sha256 };
        }
        if (completion.output.kind === 'file_artifact' && completion.result === 'verified'
            && typeof value.file_name === 'string' && digest(value.digest_sha256)
            && Number.isSafeInteger(value.size_bytes) && (value.size_bytes as number) >= 0) {
            return { operation: 'create', verified: true, fileName: value.file_name, bytes: value.size_bytes as number, digest: value.digest_sha256 };
        }
        if (completion.output.kind !== 'text_file_mutation' || !['update', 'delete'].includes(String(value.operation))
            || typeof value.original_file_name !== 'string' || !value.original_file_name
            || !Number.isSafeInteger(value.original_size_bytes) || (value.original_size_bytes as number) < 0
            || typeof value.verified !== 'boolean' || !digest(value.original_sha256)
            || !record(value.recovery) || !digest(value.recovery.recovery_id)
            || !Number.isSafeInteger(value.recovery.created_at_unix_ms) || (value.recovery.created_at_unix_ms as number) <= 0
            || !Number.isSafeInteger(value.recovery.expires_at_unix_ms) || (value.recovery.expires_at_unix_ms as number) > 8.64e15
            || (value.recovery.expires_at_unix_ms as number) <= (value.recovery.created_at_unix_ms as number)
            || typeof value.recovery.cleanup_pending !== 'boolean'
            || completion.result !== (value.verified ? 'verified' : 'outcome_unknown')) return null;
        const recovery = { id: value.recovery.recovery_id, expiresAt: value.recovery.expires_at_unix_ms as number, cleanupPending: value.recovery.cleanup_pending };
        const updated = value.updated_file;
        if (value.verified && value.operation === 'update') {
            if (updated === null || updated === undefined) return { operation: 'update', verified: true, fileName: value.original_file_name, recovery, referenceUnavailable: true };
            if (!record(updated) || typeof updated.file_name !== 'string' || !digest(updated.digest_sha256)
                || !Number.isSafeInteger(updated.size_bytes) || (updated.size_bytes as number) < 0) return null;
            return { operation: 'update', verified: true, fileName: updated.file_name, bytes: updated.size_bytes as number, digest: updated.digest_sha256, recovery };
        }
        if (updated !== null && updated !== undefined) return null;
        return { operation: value.operation as 'update' | 'delete', verified: value.verified, fileName: value.original_file_name, bytes: value.original_size_bytes as number, digest: value.original_sha256, recovery };
    } catch { return null; }
}

export function AssistantFileResult({ receipt, text, onExportBackup }: { receipt: FileReceipt; text: string; onExportBackup?: (id: string) => Promise<void> }) {
    const { t } = useTranslation();
    const busyRef = useRef(false);
    const [exporting, setExporting] = useState(false);
    const [exportFailed, setExportFailed] = useState<string | null>(null);
    const download = async () => {
        if (!receipt.recovery || !onExportBackup || busyRef.current) return;
        busyRef.current = true;
        setExporting(true); setExportFailed(null);
        try { await onExportBackup(receipt.recovery.id); }
        catch (error) { setExportFailed(recoveryErrorKey(error)); }
        finally { busyRef.current = false; setExporting(false); }
    };
    return <Disclosure className="min-w-0" title={<>{t('pages.aiAssistant.fileReceipt.title')} · {t(`pages.aiAssistant.fileReceipt.${receipt.operation}`)} · {t(`pages.aiAssistant.fileReceipt.${receipt.verified ? 'verified' : 'unknown'}`)}</>} summaryClassName="cursor-pointer font-medium">

        <dl className="mt-3 space-y-2 break-words">
            {receipt.fileName && <div><dt>{t('pages.aiAssistant.fileReceipt.name')}</dt><dd>{receipt.fileName}</dd></div>}
            {receipt.bytes !== undefined && <div><dt>{t('pages.aiAssistant.fileReceipt.bytes')}</dt><dd>{receipt.bytes}</dd></div>}
            {receipt.digest && <div><dt>SHA-256</dt><dd className="break-all font-mono text-xs">{receipt.digest}</dd></div>}
            {receipt.referenceUnavailable && <p>{t('pages.aiAssistant.fileReceipt.referenceUnavailable')}</p>}
            {receipt.recovery && <div><dt>{t('pages.aiAssistant.fileReceipt.recovery')}</dt><dd>{t('pages.aiAssistant.fileReceipt.backupUntil', { time: formatLocalTime(new Date(receipt.recovery.expiresAt).toISOString()) })}</dd><p className="mt-1 text-xs text-muted-foreground">{t('pages.aiAssistant.fileReceipt.recoveryHint')}</p>{receipt.recovery.cleanupPending && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.fileReceipt.cleanupPending')}</p>}</div>}
            {!receipt.verified && <p className="text-amber-700 dark:text-amber-300">{t('pages.aiAssistant.fileReceipt.unknownHint')}</p>}
        </dl>
        {receipt.recovery && onExportBackup && <div className="mt-3 space-y-2">
            <Button type="button" size="sm" variant="outline" disabled={exporting} onClick={() => void download()}>
                {exporting ? <Loader2 className="size-4 animate-spin" /> : <Download className="size-4" />}{t('pages.fileRecovery.export')}
            </Button>
            {exportFailed && <p role="alert" className="text-sm text-destructive">{t(`pages.fileRecovery.${exportFailed}`)}</p>}
        </div>}
        <Disclosure className="mt-3" title={<>{t('pages.aiAssistant.commandReceipt.raw')}</>} summaryClassName="cursor-pointer text-xs"><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all text-xs">{text}</pre></Disclosure>
    </Disclosure>;
}
