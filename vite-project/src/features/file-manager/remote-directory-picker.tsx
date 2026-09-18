import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Folder } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useFileTransfer } from './use-file-transfer';

type Directory = { name: string; path: string; err_msg?: string | null };
const pageSize = 100;

export function RemoteDirectoryPicker({ deskId, sessionTargetId, disabled, onSelect, onCancel }: {
    deskId: string;
    sessionTargetId: string | null;
    disabled: boolean;
    onSelect: (path: string) => boolean;
    onCancel: () => void;
}) {
    const { t } = useTranslation();
    const { listFiles, querySystemInfo, closeConnection } = useFileTransfer(deskId, undefined, sessionTargetId);
    const [paths, setPaths] = useState<string[] | null>(null);
    const [page, setPage] = useState(1);
    const [reload, setReload] = useState(0);
    const [data, setData] = useState<{ file_info_list: Directory[]; total_count: number } | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const path = paths?.at(-1);
    useEffect(() => () => closeConnection(), [closeConnection]);
    useEffect(() => {
        let active = true;
        setLoading(true);
        setError(null);
        setData(null);
        void (async () => {
            try {
                if (path === undefined) {
                    const info = await querySystemInfo();
                    if (!info.name) throw new Error(t('pages.aiAssistant.directories.platformUnavailable'));
                    if (active) setPaths([/windows/i.test(info.name) ? '' : '/']);
                    return;
                }
                const result = await listFiles({ path, page_no: page, page_count: pageSize, directories_only: true });
                if (active) { setData(result); setLoading(false); }
            } catch (reason) {
                if (active) { setError(reason instanceof Error ? reason.message : t('common.unknownError')); setLoading(false); }
            }
        })();
        return () => { active = false; };
    }, [path, page, reload, listFiles, querySystemInfo, t]);
    const busy = disabled || loading;
    const navigate = (next: string[]) => { setLoading(true); setData(null); setPaths(next); setPage(1); };
    const changePage = (next: number) => { setLoading(true); setData(null); setPage(next); };
    return <div className="space-y-2 rounded-md border p-3" aria-busy={loading}>
        <p className="break-all text-sm font-mono">{path || t('pages.aiAssistant.directories.root')}</p>
        <div className="flex flex-wrap gap-2">
            <Button type="button" size="sm" variant="outline" disabled={busy || !paths || paths.length < 2}
                onClick={() => navigate(paths!.slice(0, 1))}>{t('pages.aiAssistant.directories.root')}</Button>
            <Button type="button" size="sm" variant="outline" disabled={busy || !paths || paths.length < 2}
                onClick={() => navigate(paths!.slice(0, -1))}>{t('pages.aiAssistant.directories.up')}</Button>
            <Button type="button" size="sm" variant="outline" disabled={disabled || loading}
                onClick={() => setReload(n => n + 1)}>{t('common.refresh')}</Button>
        </div>
        {loading && <p role="status">{t('common.loading')}</p>}
        {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
        {data && <>
            <div className="max-h-64 space-y-1 overflow-y-auto">
                {data.file_info_list.map(directory => <Button key={directory.path} type="button" variant="ghost"
                    className="flex w-full justify-start" disabled={busy || !!directory.err_msg} title={directory.err_msg ?? directory.path}
                    onClick={() => navigate([...(paths ?? []), directory.path])}>
                    <Folder className="mr-2 h-4 w-4 shrink-0" /><span className="truncate">{directory.name}</span>
                </Button>)}
                {!data.file_info_list.length && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.directories.noChildren')}</p>}
            </div>
            <div className="flex items-center gap-2">
                <Button type="button" size="sm" variant="outline" disabled={busy || page <= 1} onClick={() => changePage(page - 1)}>{t('pages.aiAssistant.directories.previous')}</Button>
                <span className="text-xs">{page} / {Math.max(1, Math.ceil(data.total_count / pageSize))}</span>
                <Button type="button" size="sm" variant="outline" disabled={busy || page * pageSize >= data.total_count} onClick={() => changePage(page + 1)}>{t('pages.aiAssistant.directories.next')}</Button>
            </div>
        </>}
        <div className="flex gap-2">
            <Button type="button" disabled={busy || !path || !data || !!error} onClick={() => onSelect(path!)}>{t('pages.aiAssistant.directories.select')}</Button>
            <Button type="button" variant="outline" onClick={onCancel}>{t('common.cancel')}</Button>
        </div>
    </div>;
}
