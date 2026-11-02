import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';
import { useTranslation } from 'react-i18next';
import { v4 as uuidv4 } from 'uuid';
import { ChevronDown, ChevronUp, Upload, X, RotateCcw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Progress } from '@/components/ui/progress';
import { formatBytes } from '@/lib/utils';
import { deskErrorMessage } from '@/lib/desk-error-i18n';
import { deskErrorCodeEnum } from '@/services/types';
import { UploadController, initialUpload, type UploadProgress } from '../file-manager/upload-controller';
import { supportsFileTransfer } from './file-transfer-support';

type Row = Omit<UploadProgress, 'status'> & { status: UploadProgress['status'] | 'queued' };
const active = (row: Row) => ['queued', 'connecting', 'awaiting_approval', 'transferring', 'confirming'].includes(row.status);

export function useDeskFileUpload({ channel, platform, enabled, root, releaseInputs }: {
    channel: RTCDataChannel | null;
    platform: unknown;
    enabled: boolean;
    root: RefObject<HTMLDivElement | null>;
    releaseInputs: () => void;
}) {
    const { t } = useTranslation();
    const [rows, setRows] = useState<Row[]>([]);
    const [dragging, setDragging] = useState(false);
    const [notice, setNotice] = useState('');
    const [ready, setReady] = useState(channel?.readyState === 'open');
    const [collapsed, setCollapsed] = useState(() => window.innerWidth < 640);
    const [showLocation, setShowLocation] = useState(false);
    const [interacting, setInteracting] = useState(false);
    const [hovered, setHovered] = useState(false);
    const [panelHeight, setPanelHeight] = useState<number>();
    const controller = useRef<UploadController | null>(null);
    const queue = useRef<{ id: string; file: File }[]>([]);
    const files = useRef(new Map<string, File>());
    const running = useRef(false);
    const compact = useRef(false);
    const pump = useRef<() => void>(() => {});
    const patch = useCallback((row: Row) => setRows(previous => previous.some(item => item.transferId === row.transferId)
        ? previous.map(item => item.transferId === row.transferId ? row : item) : [...previous, row]), []);
    pump.current = () => {
        if (running.current) return;
        const owner = controller.current;
        if (!owner) return;
        running.current = true;
        void (async () => {
            try {
                while (controller.current === owner && queue.current.length) {
                    const entry = queue.current.shift()!;
                    try { await owner.upload(entry.file, { target_location: 'downloads' }, entry.id); }
                    catch (error) { patch({ ...initialUpload(entry.file, entry.id), status: 'error', errorMessage: String(error) }); }
                }
            } finally {
                running.current = false;
                if (queue.current.length) pump.current();
            }
        })();
    };

    useEffect(() => {
        setReady(channel?.readyState === 'open');
        const owner = channel ? new UploadController(channel, patch) : null;
        controller.current = owner;
        const open = () => setReady(true);
        const close = () => {
            setReady(false);
            queue.current = [];
            files.current.clear();
            setRows(previous => previous.map(row => row.status === 'queued' ? { ...row, status: 'cancelled' } : row));
        };
        channel?.addEventListener('open', open);
        channel?.addEventListener('close', close);
        channel?.addEventListener('error', close);
        return () => {
            close();
            owner?.dispose();
            if (controller.current === owner) controller.current = null;
            channel?.removeEventListener('open', open);
            channel?.removeEventListener('close', close);
            channel?.removeEventListener('error', close);
        };
    }, [channel, patch]);

    const enqueue = useCallback((file: File) => {
        const id = uuidv4();
        files.current.set(id, file);
        queue.current.push({ id, file });
        patch({ ...initialUpload(file, id), status: 'queued' });
        pump.current();
    }, [patch]);
    const reason = !platform ? 'notReady' : !supportsFileTransfer(platform) ? 'unsupported' : !enabled ? 'forbidden' : !ready ? 'notReady' : null;

    useEffect(() => {
        const host = root.current;
        if (!host) return;
        const measure = () => {
            const { width, height } = host.getBoundingClientRect();
            if (!width || !height) return;
            setPanelHeight(height / 2);
            const next = width < 600 || height < 400;
            if (next && !compact.current) setCollapsed(true);
            compact.current = next;
        };
        measure();
        const observer = new ResizeObserver(measure);
        observer.observe(host);
        return () => observer.disconnect();
    }, [root]);

    useEffect(() => {
        const host = root.current;
        if (!host) return;
        let depth = 0;
        const isFile = (event: DragEvent) => Array.from(event.dataTransfer?.types ?? []).includes('Files');
        const ignored = (event: DragEvent) => event.target instanceof Element && Boolean(event.target.closest('[data-file-drop-ignore]'));
        const enter = (event: DragEvent) => {
            if (!isFile(event) || ignored(event)) return;
            event.preventDefault();
            depth++;
            releaseInputs();
            setDragging(true);
        };
        const over = (event: DragEvent) => {
            if (!isFile(event) || ignored(event)) return;
            event.preventDefault();
            if (event.dataTransfer) event.dataTransfer.dropEffect = reason ? 'none' : 'copy';
        };
        const leave = (event: DragEvent) => {
            if (!depth || ignored(event)) return;
            if (--depth <= 0) { depth = 0; setDragging(false); }
        };
        const reset = () => { depth = 0; setDragging(false); };
        const drop = (event: DragEvent) => {
            if (!isFile(event) || ignored(event)) return;
            event.preventDefault();
            event.stopPropagation();
            reset();
            if (reason) { setNotice(t(`pages.desk.fileUpload.${reason}`)); return; }
            const items = Array.from(event.dataTransfer?.items ?? []);
            let directories = false;
            for (const item of items) {
                if (item.kind !== 'file') continue;
                if (item.webkitGetAsEntry?.()?.isDirectory) { directories = true; continue; }
                const file = item.getAsFile();
                if (file) enqueue(file);
            }
            if (!items.length) for (const file of Array.from(event.dataTransfer?.files ?? [])) enqueue(file);
            if (directories) setNotice(t('pages.desk.fileUpload.folder'));
        };
        const preventNavigation = (event: DragEvent) => { if (event.type === 'drop') reset(); if (isFile(event)) event.preventDefault(); };
        host.addEventListener('dragenter', enter); host.addEventListener('dragover', over);
        host.addEventListener('dragleave', leave); host.addEventListener('drop', drop);
        document.addEventListener('dragover', preventNavigation); document.addEventListener('drop', preventNavigation);
        window.addEventListener('blur', reset); window.addEventListener('dragend', reset);
        return () => {
            host.removeEventListener('dragenter', enter); host.removeEventListener('dragover', over);
            host.removeEventListener('dragleave', leave); host.removeEventListener('drop', drop);
            document.removeEventListener('dragover', preventNavigation); document.removeEventListener('drop', preventNavigation);
            window.removeEventListener('blur', reset); window.removeEventListener('dragend', reset);
        };
    }, [root, reason, releaseInputs, enqueue, t]);

    useEffect(() => {
        if (!notice) return;
        const timer = setTimeout(() => setNotice(''), 5000);
        return () => clearTimeout(timer);
    }, [notice]);
    useEffect(() => {
        if (interacting || hovered || !rows.length || rows.some(row => row.status !== 'completed')) return;
        const timer = setTimeout(() => setCollapsed(true), 5000);
        return () => clearTimeout(timer);
    }, [rows, interacting, hovered]);

    const remove = (id: string) => {
        files.current.delete(id);
        setRows(previous => previous.filter(row => row.transferId !== id || active(row)));
    };
    const cancel = (row: Row) => {
        if (row.status === 'queued') {
            queue.current = queue.current.filter(item => item.id !== row.transferId);
            patch({ ...row, status: 'cancelled' });
        } else controller.current?.cancel(row.transferId);
    };
    const status = (row: Row) => t(`pages.desk.fileUpload.${row.status}`);
    const summary = rows.every(row => row.status === 'completed') ? t('pages.desk.fileUpload.success', { count: rows.length })
        : `${t('pages.desk.fileUpload.title')} · ${rows.length}`;
    const issues = rows.filter(row => row.status === 'error' || row.status === 'unconfirmed').length;

    const dropOverlay = dragging || notice ? <div className="pointer-events-none absolute inset-3 z-[65] flex items-center justify-center rounded-lg border-2 border-primary bg-background/70 text-foreground" role="status">
        <div className="max-w-full px-4 text-center"><Upload className="mx-auto mb-2 h-8 w-8" />
            <p className="font-medium">{notice || t(`pages.desk.fileUpload.${reason ?? 'drop'}`)}</p>
            {!notice && !reason && <p className="mt-1 text-sm text-muted-foreground">{t('pages.desk.fileUpload.downloads')}</p>}
        </div>
    </div> : null;

    const uploadPanel = rows.length ? <section data-file-drop-ignore role="region" aria-label={t('pages.desk.fileUpload.title')}
        className="pointer-events-auto flex max-h-full min-h-0 flex-col overflow-hidden rounded-lg border bg-popover text-popover-foreground shadow-xl"
        style={{ maxHeight: panelHeight }}
        onPointerDown={event => { releaseInputs(); event.stopPropagation(); }} onPointerUp={event => event.stopPropagation()}
        onMouseDown={event => event.stopPropagation()} onMouseUp={event => event.stopPropagation()}
        onKeyDown={event => event.stopPropagation()} onKeyUp={event => event.stopPropagation()}
        onClick={event => event.stopPropagation()} onPointerMove={event => event.stopPropagation()}
        onWheel={event => event.stopPropagation()} onTouchStart={event => event.stopPropagation()} onTouchMove={event => event.stopPropagation()} onTouchEnd={event => event.stopPropagation()}
        onFocusCapture={() => { releaseInputs(); setInteracting(true); }} onBlurCapture={event => { if (!event.currentTarget.contains(event.relatedTarget)) setInteracting(false); }}
        onMouseEnter={() => setHovered(true)} onMouseLeave={() => setHovered(false)}>
        <header className="flex shrink-0 items-center gap-2 px-3 py-1">
            <Upload className="h-4 w-4 shrink-0" /><div className="min-w-0 flex-1 text-sm" aria-live="polite"><p>{summary}</p>
                {collapsed && <p className="truncate text-xs text-muted-foreground">{rows.find(active) ? status(rows.find(active)!) : ''}{issues > 0 && ` · ${t('pages.desk.fileUpload.issues', { count: issues })}`}</p>}</div>
            <Button variant="ghost" size="icon" className="h-11 w-11 shrink-0" aria-expanded={!collapsed} aria-label={t(`pages.desk.fileUpload.${collapsed ? 'expand' : 'collapse'}`)} onClick={() => setCollapsed(!collapsed)}>{collapsed ? <ChevronDown className="h-4 w-4" /> : <ChevronUp className="h-4 w-4" />}</Button>
        </header>
        {!collapsed && <div className="min-h-0 overflow-y-auto"><ul className="px-3">
            {rows.map(row => <li key={row.transferId} className="border-t py-2 text-sm">
                <div className="flex items-center gap-2"><span className="min-w-0 flex-1 truncate" title={row.fileName} tabIndex={0}>{row.resolvedFileName ?? row.fileName}</span>
                    {active(row) && row.status !== 'confirming' ? <Button variant="ghost" size="icon" className="h-11 w-11 shrink-0" aria-label={t('pages.desk.fileUpload.cancelFile', { name: row.fileName })} onClick={() => cancel(row)}><X className="h-4 w-4" /></Button>
                        : !active(row) && <Button variant="ghost" size="icon" className="h-11 w-11 shrink-0" aria-label={t('pages.desk.fileUpload.removeFile', { name: row.fileName })} onClick={() => remove(row.transferId)}><X className="h-4 w-4" /></Button>}
                </div>
                <p className="text-xs text-muted-foreground">{status(row)} · {formatBytes(row.transferredBytes)} / {formatBytes(row.fileSize)}</p>
                {row.status === 'transferring' && <Progress className="mt-2 h-1" value={row.progress} />}
                {row.status === 'error' && <p className="mt-1 break-words text-xs text-destructive">{deskErrorMessage(t, {
                    [deskErrorCodeEnum.TIMEOUT]: 'pages.fileError.transferStalled',
                    [deskErrorCodeEnum.PERMISSION_ERROR]: 'pages.fileError.transferDenied',
                }, row.errorCode, row.errorMessage, t('pages.desk.fileUpload.error'))}</p>}
                {row.status === 'error' && ready && !reason && files.current.has(row.transferId) && <Button variant="ghost" size="sm" className="min-h-11" onClick={() => { const file = files.current.get(row.transferId)!; remove(row.transferId); enqueue(file); }}><RotateCcw className="mr-1 h-3 w-3" />{t('pages.desk.fileUpload.retry')}</Button>}
                {showLocation && row.resolvedTargetDir && <p className="mt-1 break-all text-xs text-muted-foreground">{row.resolvedTargetDir}/{row.resolvedFileName ?? row.fileName}</p>}
            </li>)}
        </ul><footer className="flex shrink-0 flex-wrap justify-between border-t px-2 py-1">
            {rows.some(row => row.resolvedTargetDir) && <Button variant="ghost" size="sm" className="min-h-11" aria-expanded={showLocation} onClick={() => setShowLocation(!showLocation)}>{t('pages.desk.fileUpload.location')}</Button>}
            <Button variant="ghost" size="sm" className="min-h-11" disabled={rows.every(active)} onClick={() => { rows.filter(row => !active(row)).forEach(row => files.current.delete(row.transferId)); setRows(previous => previous.filter(active)); }}>{t('pages.desk.fileUpload.clear')}</Button>
        </footer></div>}
    </section> : null;

    return { dropOverlay, uploadPanel };
}
