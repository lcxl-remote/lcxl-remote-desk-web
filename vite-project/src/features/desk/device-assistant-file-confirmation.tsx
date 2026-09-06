import { useTranslation } from 'react-i18next';

interface TextFileReview {
    fileName: string;
    fileResultCallId: string;
    expectedSha256: string;
    operation: 'update' | 'delete';
    change?: { kind: 'replace_all' | 'replace_once'; content_utf8?: string; before?: string; after?: string } | null;
    oneShot: boolean;
    recoverable: boolean;
}

export function validTextFileReview(input: unknown): input is TextFileReview {
    if (!input || typeof input !== 'object') return false;
    const v = input as TextFileReview;
    if (typeof v.fileName !== 'string' || !v.fileName || typeof v.fileResultCallId !== 'string' || !v.fileResultCallId
        || !/^[0-9a-f]{64}$/.test(v.expectedSha256) || v.oneShot !== true || v.recoverable !== true) return false;
    if (v.operation === 'delete') return v.change == null;
    return v.operation === 'update' && (v.change?.kind === 'replace_all' ? typeof v.change.content_utf8 === 'string'
        : v.change?.kind === 'replace_once' && typeof v.change.before === 'string' && Boolean(v.change.before)
            && typeof v.change.after === 'string');
}

export function fileApprovalBlocked(item: { toolName: string; textFileConfirmation?: unknown }): boolean {
    if (!['update_text_file', 'delete_text_file'].includes(item.toolName)) return false;
    return !validTextFileReview(item.textFileConfirmation)
        || item.textFileConfirmation.operation !== (item.toolName === 'delete_text_file' ? 'delete' : 'update');
}

export function TextFileConfirmationCard({ value }: { value: TextFileReview }) {
    const { t } = useTranslation();
    const block = (title: string, text: string) => <div><p>{t(title)}</p>
        <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2">{text || t('pages.deviceAssistant.fileConfirmEmpty')}</pre></div>;
    return <div data-testid="text-file-confirmation" className="mt-3 max-w-full space-y-2 rounded-md border p-3 text-xs">
        <p className="font-semibold">{t(value.operation === 'delete' ? 'pages.deviceAssistant.fileConfirmDelete' : 'pages.deviceAssistant.fileConfirmUpdate')}: {value.fileName}</p>
        <p>{t('pages.deviceAssistant.fileConfirmOneShot')}</p>
        {value.operation === 'delete' && <p>{t('pages.deviceAssistant.fileConfirmRecoverable')}</p>}
        {value.change?.kind === 'replace_once' && <>{block('pages.deviceAssistant.fileConfirmBefore', value.change.before!)}{block('pages.deviceAssistant.fileConfirmAfter', value.change.after!)}</>}
        {value.change?.kind === 'replace_all' && block('pages.deviceAssistant.fileConfirmReplaceAll', value.change.content_utf8!)}
        <p className="break-all">SHA-256: {value.expectedSha256}</p>
        <p className="break-all">file_result_call_id: {value.fileResultCallId}</p>
    </div>;
}
