import { useTranslation } from 'react-i18next';

type FileReceipt = { operation: 'create' | 'update' | 'delete'; verified: boolean; fileName?: string; bytes?: number; digest: string; recoveryPath?: string };
const record = (value: unknown): value is Record<string, unknown> => typeof value === 'object' && value !== null && !Array.isArray(value);
const digest = (value: unknown): value is string => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);

export function parseFileReceipt(text: string): FileReceipt | null {
    try {
        const completion: unknown = JSON.parse(text);
        if (!record(completion) || !record(completion.output) || !record(completion.output.value)) return null;
        const value = completion.output.value;
        if (completion.output.kind === 'file_artifact' && completion.result === 'verified'
            && typeof value.file_name === 'string' && digest(value.digest_sha256)
            && Number.isSafeInteger(value.size_bytes) && (value.size_bytes as number) >= 0) {
            return { operation: 'create', verified: true, fileName: value.file_name, bytes: value.size_bytes as number, digest: value.digest_sha256 };
        }
        if (completion.output.kind !== 'text_file_mutation' || !['update', 'delete'].includes(String(value.operation))
            || typeof value.original_file_name !== 'string' || !value.original_file_name
            || !Number.isSafeInteger(value.original_size_bytes) || (value.original_size_bytes as number) < 0
            || typeof value.verified !== 'boolean' || !digest(value.original_sha256)
            || typeof value.recovery_path !== 'string' || !value.recovery_path || value.recovery_path.length > 4096
            || completion.result !== (value.verified ? 'verified' : 'outcome_unknown')) return null;
        const updated = value.updated_file;
        if (value.verified && value.operation === 'update') {
            if (!record(updated) || typeof updated.file_name !== 'string' || !digest(updated.digest_sha256)
                || !Number.isSafeInteger(updated.size_bytes) || (updated.size_bytes as number) < 0) return null;
            return { operation: 'update', verified: true, fileName: updated.file_name, bytes: updated.size_bytes as number, digest: updated.digest_sha256, recoveryPath: value.recovery_path };
        }
        if (updated !== null && updated !== undefined) return null;
        return { operation: value.operation as 'update' | 'delete', verified: value.verified, fileName: value.original_file_name, bytes: value.original_size_bytes as number, digest: value.original_sha256, recoveryPath: value.recovery_path };
    } catch { return null; }
}

export function AssistantFileResult({ receipt, text }: { receipt: FileReceipt; text: string }) {
    const { t } = useTranslation();
    return <details className="min-w-0">
        <summary className="cursor-pointer font-medium">{t('pages.deviceAssistant.fileReceipt.title')} · {t(`pages.deviceAssistant.fileReceipt.${receipt.operation}`)} · {t(`pages.deviceAssistant.fileReceipt.${receipt.verified ? 'verified' : 'unknown'}`)}</summary>
        <dl className="mt-3 space-y-2 break-words">
            {receipt.fileName && <div><dt>{t('pages.deviceAssistant.fileReceipt.name')}</dt><dd>{receipt.fileName}</dd></div>}
            {receipt.bytes !== undefined && <div><dt>{t('pages.deviceAssistant.fileReceipt.bytes')}</dt><dd>{receipt.bytes}</dd></div>}
            <div><dt>SHA-256</dt><dd className="break-all font-mono text-xs">{receipt.digest}</dd></div>
            {receipt.recoveryPath && <div><dt>{t('pages.deviceAssistant.fileReceipt.recovery')}</dt><dd className="break-all font-mono text-xs">{receipt.recoveryPath}</dd><p className="mt-1 text-xs text-muted-foreground">{t('pages.deviceAssistant.fileReceipt.recoveryHint')}</p></div>}
            {!receipt.verified && <p className="text-amber-700 dark:text-amber-300">{t('pages.deviceAssistant.fileReceipt.unknownHint')}</p>}
        </dl>
        <details className="mt-3"><summary className="cursor-pointer text-xs">{t('pages.deviceAssistant.commandReceipt.raw')}</summary><pre className="max-h-64 overflow-auto whitespace-pre-wrap break-all text-xs">{text}</pre></details>
    </details>;
}
