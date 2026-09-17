import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Paperclip } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog';
import { Textarea } from '@/components/ui/textarea';
import { deleteAssistantAttachments, getAssistantAttachment, listAssistantAttachments, readAssistantAttachment } from '@/services/clients';
import type { AttachmentDto, AttachmentListDto, AttachmentPageDto } from '@/services/types';

export function AssistantAttachments({ sessionId, attachmentIds }: { sessionId?: string | null; attachmentIds?: string[] }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    // Remount the panel for every subject change, including a cleared session.
    return <Dialog open={open} onOpenChange={setOpen}>
        <Button variant="ghost" size="sm" className="assistant-action" disabled={!sessionId}
            aria-label={t('pages.deviceAssistant.attachments.title')} onClick={() => setOpen(true)}>
            <Paperclip className="h-4 w-4 shrink-0" aria-hidden="true" />
            <span className="assistant-action-label">{t('pages.deviceAssistant.attachments.title')}</span>
        </Button>
        <DialogContent className="flex max-h-[85vh] max-w-4xl flex-col overflow-hidden">
            <DialogTitle>{t('pages.deviceAssistant.attachments.title')}</DialogTitle>
            {open && sessionId && <AttachmentPanel key={sessionId} sessionId={sessionId} attachmentIds={attachmentIds} />}
        </DialogContent>
    </Dialog>;
}

function AttachmentPanel({ sessionId, attachmentIds }: { sessionId: string; attachmentIds?: string[] }) {
    const { t } = useTranslation();
    const label = (key: string) => t(`pages.deviceAssistant.attachments.${key}`);
    const [list, setList] = useState<AttachmentListDto | null>(null);
    const [kind, setKind] = useState('all');
    const [status, setStatus] = useState('all');
    const [selected, setSelected] = useState<Set<string>>(new Set());
    const [detail, setDetail] = useState<AttachmentDto | null>(null);
    const [page, setPage] = useState<AttachmentPageDto | null>(null);
    const [image, setImage] = useState<string | null>(null);
    const [queries, setQueries] = useState('');
    const [activeQueries, setActiveQueries] = useState<string[] | null>(null);
    const [busy, setBusy] = useState(false);
    const [failed, setFailed] = useState(false);
    const lifetime = useRef(new AbortController());
    const detailRequest = useRef<AbortController | null>(null);
    const imageUrl = useRef<string | null>(null);
    const listGeneration = useRef(0);
    const detailId = useRef<string | null>(null);

    const clearDetail = () => {
        detailRequest.current?.abort();
        detailId.current = null;
        setBusy(false);
        if (imageUrl.current) URL.revokeObjectURL(imageUrl.current);
        imageUrl.current = null;
        setImage(null); setPage(null); setDetail(null); setQueries(''); setActiveQueries(null);
    };
    const load = async (before?: string) => {
        const generation = ++listGeneration.current;
        const signal = lifetime.current.signal;
        try {
            const response = await listAssistantAttachments({ session: sessionId, before }, { signal });
            if (signal.aborted || generation !== listGeneration.current) return;
            if (!response.success || !response.data) throw new Error('Attachment list unavailable');
            const data = response.data;
            setList(previous => before && previous ? { ...data, attachments: [...new Map([...previous.attachments, ...data.attachments].map(item => [item.attachment_id, item])).values()] } : data);
            if (data.attachments.some(item => item.attachment_id === detailId.current && item.status !== 'available')) clearDetail();
        } catch { if (!signal.aborted) setFailed(true); }
    };
    useEffect(() => {
        lifetime.current = new AbortController();
        void load();
        return () => {
            lifetime.current.abort(); detailRequest.current?.abort();
            if (imageUrl.current) URL.revokeObjectURL(imageUrl.current);
        };
        // The panel is keyed by sessionId; all async operations share its lifetime.
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [sessionId]);

    const contentBlob = async (item: AttachmentDto, signal: AbortSignal) => {
        const data: unknown = await getAssistantAttachment({ session: sessionId, attachment: item.attachment_id }, { responseType: 'blob', signal });
        // Success is always binary; business errors are JSON, including HTTP 200.
        if (!(data instanceof Blob) || data.type !== 'application/octet-stream') throw new Error('Attachment unavailable');
        return data;
    };
    const view = async (item: AttachmentDto, cursor?: string, search: string[] | null = null) => {
        detailRequest.current?.abort();
        const controller = new AbortController();
        detailRequest.current = controller;
        if (!cursor) {
            if (imageUrl.current) URL.revokeObjectURL(imageUrl.current);
            imageUrl.current = null; setImage(null); setPage(null);
        }
        detailId.current = item.attachment_id;
        setDetail(item); setBusy(true); setFailed(false); setActiveQueries(search);
        try {
            if (item.kind === 'image') {
                const blob = await contentBlob(item, controller.signal);
                if (controller.signal.aborted || lifetime.current.signal.aborted) return;
                imageUrl.current = URL.createObjectURL(new Blob([blob], { type: item.media_type }));
                setImage(imageUrl.current);
            } else {
                const response = await readAssistantAttachment({
                    session: sessionId, attachment_id: item.attachment_id, cursor,
                    queries: search, ignore_case: true, before_context: 2, after_context: 2,
                }, { signal: controller.signal });
                if (controller.signal.aborted || lifetime.current.signal.aborted) return;
                if (!response.success || !response.data) throw new Error('Attachment unavailable');
                setPage(response.data);
            }
        } catch {
            if (!controller.signal.aborted && !lifetime.current.signal.aborted) {
                setPage(null); setFailed(true); void load();
            }
        } finally {
            if (!controller.signal.aborted && !lifetime.current.signal.aborted) setBusy(false);
        }
    };
    const download = async (item: AttachmentDto) => {
        setBusy(true); setFailed(false);
        const signal = lifetime.current.signal;
        try {
            const blob = await contentBlob(item, signal);
            if (signal.aborted) return;
            const url = URL.createObjectURL(blob);
            const anchor = document.createElement('a');
            anchor.href = url; anchor.download = `attachment.${item.kind === 'json' ? 'json' : item.kind === 'text' ? 'txt' : item.media_type === 'image/jpeg' ? 'jpg' : 'png'}`;
            anchor.click(); URL.revokeObjectURL(url);
        } catch { if (!signal.aborted) { setFailed(true); void load(); } }
        finally { if (!signal.aborted) setBusy(false); }
    };
    const remove = async (ids: string[]) => {
        if (!ids.length || !window.confirm(label('deleteConfirm'))) return;
        setBusy(true); setFailed(false);
        const signal = lifetime.current.signal;
        try {
            const response = await deleteAssistantAttachments({ session: sessionId, attachment_ids: ids }, { signal });
            if (signal.aborted) return;
            if (!response.success || !response.data) throw new Error('Delete failed');
            if (detail && ids.includes(detail.attachment_id)) clearDetail();
            setSelected(new Set()); await load();
        } catch { if (!signal.aborted) { setFailed(true); void load(); } }
        finally { if (!signal.aborted) setBusy(false); }
    };
    let formattedJson: string | null = null;
    if (detail?.kind === 'json' && page && !page.json_fragment) {
        try { formattedJson = JSON.stringify(JSON.parse(page.lines.map(line => line.text).join('')), null, 2); }
        catch { /* An invalid or incomplete document is displayed as original text. */ }
    }
    const visible = list?.attachments.filter(item => (!attachmentIds || attachmentIds.includes(item.attachment_id)) && (kind === 'all' || item.kind === kind) && (status === 'all' || item.status === status)) ?? [];
    return <div className="flex min-h-0 flex-col gap-3 overflow-y-auto assistant-scrollbar">
        {list && <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.attachments.usage', { used: (list.used_bytes / 1048576).toFixed(2), capacity: list.capacity_bytes / 1048576 })}</p>}
        <div className="flex flex-wrap items-center gap-2">
            <select aria-label={label('type')} className="rounded border bg-background p-2 text-sm" value={kind} onChange={event => setKind(event.target.value)}>
                {['all', 'image', 'text', 'json'].map(value => <option key={value} value={value}>{label(value)}</option>)}
            </select>
            <select aria-label={label('status')} className="rounded border bg-background p-2 text-sm" value={status} onChange={event => setStatus(event.target.value)}>
                {['all', 'available', 'deleted', 'evicted'].map(value => <option key={value} value={value}>{label(value)}</option>)}
            </select>
            <Button variant="outline" size="sm" disabled={busy} onClick={() => { setSelected(new Set()); setFailed(false); void load(); }}>{label('refresh')}</Button>
            <Button variant="outline" size="sm" disabled={busy || !selected.size} onClick={() => void remove([...selected])}>{label('deleteSelected')} ({selected.size}/32)</Button>
        </div>
        {failed && <p role="alert" className="text-sm text-destructive">{label('failed')}</p>}
        {!list && !failed && <p role="status">{label('loading')}</p>}
        {list && !visible.length && <p className="text-sm text-muted-foreground">{label('empty')}</p>}
        <ul className="space-y-2">
            {visible.map(item => <li key={item.attachment_id} className="flex flex-wrap items-center gap-2 rounded border p-2 text-sm">
                <input type="checkbox" aria-label={`${label('select')} ${item.attachment_id}`} checked={selected.has(item.attachment_id)} disabled={busy || item.status !== 'available' || (!selected.has(item.attachment_id) && selected.size >= 32)} onChange={event => setSelected(previous => {
                    const next = new Set(previous); if (event.target.checked) next.add(item.attachment_id); else next.delete(item.attachment_id); return next;
                })} />
                <div className="min-w-0 flex-1 break-all">
                    <p>{label(item.kind)} · {item.part} · {label(item.status)} · {item.stored_bytes.toLocaleString()} B</p>
                    <p className="text-xs text-muted-foreground">{new Date(item.created_at_unix_ms).toLocaleString()} · {item.tool_call_id}</p>
                    {item.source_truncated && <p>{label('sourceTruncated')}</p>}
                    {item.storage_truncated && <p>{t('pages.deviceAssistant.attachments.truncated', { original: item.original_bytes, stored: item.stored_bytes })}</p>}
                </div>
                <Button size="sm" variant="ghost" disabled={busy || item.status !== 'available'} onClick={() => { setQueries(''); void view(item); }}>{label('view')}</Button>
                <Button size="sm" variant="ghost" disabled={busy || item.status !== 'available'} onClick={() => void download(item)}>{label('download')}</Button>
                <Button size="sm" variant="ghost" disabled={busy || item.status !== 'available'} onClick={() => void remove([item.attachment_id])}>{label('delete')}</Button>
            </li>)}
        </ul>
        {list?.cursor && <Button variant="outline" disabled={busy} onClick={() => void load(list.cursor!)}>{label('more')}</Button>}
        {detail && <section className="space-y-2 border-t pt-3" aria-label={label('detail')}>
            <div className="flex items-center justify-between"><p className="text-sm">{detail.part} · {label(detail.kind)}</p><Button variant="ghost" size="sm" onClick={clearDetail}>{label('close')}</Button></div>
            {detail.kind === 'text' && <form className="flex gap-2" onSubmit={event => { event.preventDefault(); void view(detail, undefined, queries.split('\n').map(value => value.trim()).filter(Boolean)); }}>
                <Textarea rows={2} aria-label={label('queries')} value={queries} onChange={event => setQueries(event.target.value)} placeholder={label('queries')} />
                <Button size="sm" disabled={busy || !queries.trim()}>{label('search')}</Button>
                <Button type="button" size="sm" variant="outline" disabled={busy} onClick={() => void view(detail)}>{label('read')}</Button>
            </form>}
            {image && <img src={image} alt={label('image')} className="max-h-[60vh] w-full object-contain" />}
            {page && <>
                {page.json_fragment && <p className="text-xs text-muted-foreground">{label('jsonFragment')}</p>}
                {!page.lines.length && <p>{label('noMatches')}</p>}
                <div className="assistant-scrollbar max-h-[45vh] overflow-auto rounded bg-muted p-3 font-mono text-xs">
                    {formattedJson !== null ? <pre className="whitespace-pre-wrap break-all">{formattedJson}</pre> : page.lines.map(line => <div className="flex gap-3" key={`${line.line}:${line.byte_offset_in_line}`}>
                        <span className="select-none text-muted-foreground">{line.line}{line.byte_offset_in_line > 0 ? ` +${line.byte_offset_in_line}` : ''}</span>
                        <pre className="min-w-0 whitespace-pre-wrap break-all">{line.text}</pre>
                    </div>)}
                </div>
                {page.cursor && <Button size="sm" variant="outline" disabled={busy} onClick={() => void view(detail, page.cursor!, activeQueries)}>{label('nextPage')}</Button>}
            </>}
        </section>}
    </div>;
}

/** References carry no body and are scoped to the currently selected session. */
export function AssistantResultAttachments({ sessionId, text }: { sessionId?: string | null; text: string }) {
    let value: unknown;
    try { value = JSON.parse(text); } catch { return null; }
    const ids = new Set<string>();
    const visit = (node: unknown, depth: number) => {
        if (depth > 8 || ids.size >= 8 || !node || typeof node !== 'object') return;
        if ('attachment_id' in node && typeof node.attachment_id === 'string') ids.add(node.attachment_id);
        for (const child of Object.values(node)) visit(child, depth + 1);
    };
    visit(value, 0);
    return ids.size ? <AssistantAttachments sessionId={sessionId} attachmentIds={[...ids]} /> : null;
}
