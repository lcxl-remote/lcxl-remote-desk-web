import { AssistantFrameTiming } from './assistant-frame-timing';
import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import type { AiAssistantMessage } from './use-ai-assistant-chat';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog';
import { Disclosure } from '@/components/ui/disclosure';
import { deleteAssistantImage, getAssistantImage, listAssistantImages } from '@/services/clients';
import type { AiAssistantVisualEvidence } from './ai-assistant-event';

function StoredImage({ frame, onDelete }: { frame: AiAssistantVisualEvidence; onDelete: (id: string) => void }) {
    const { t } = useTranslation();
    const [url, setUrl] = useState<string | null>(null);
    const [failed, setFailed] = useState(false);
    const [open, setOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    const [requested, setRequested] = useState(false);
    // Only an explicit artifact reference denotes durable storage.
    const durable = frame.status === 'available' && frame.content?.kind === 'artifact' && frame.content.artifact_id === frame.evidence_id;
    useEffect(() => {
        if (!durable || !requested) return;
        const controller = new AbortController();
        let objectUrl: string | null = null;
        setFailed(false);
        void getAssistantImage({ session: frame.conversation_id, attachment: frame.evidence_id },
            { responseType: 'blob', signal: controller.signal }).then((data) => {
                if (controller.signal.aborted) return;
                if (!(data instanceof Blob) || !data.type.startsWith('image/')) throw new Error('Invalid image');
                objectUrl = URL.createObjectURL(data);
                setUrl(objectUrl);
            }).catch(() => { if (!controller.signal.aborted) setFailed(true); });
        return () => { controller.abort(); if (objectUrl) URL.revokeObjectURL(objectUrl); };
    }, [durable, requested, frame.conversation_id, frame.evidence_id]);
    const source = url ?? (!durable ? frame.preview_data_url : null);
    const remove = async () => {
        if (!window.confirm(t('pages.aiAssistant.imageDeleteConfirm'))) return;
        setDeleting(true);
        try {
            await deleteAssistantImage({ session: frame.conversation_id, attachment: frame.evidence_id });
            onDelete(frame.evidence_id);
        } catch { setFailed(true); } finally { setDeleting(false); }
    };
    return <div className="overflow-hidden rounded-md border bg-muted/30">
        {source ? <Button variant="unstyled" type="button" className="block w-full" onClick={() => setOpen(true)} aria-label={t('pages.aiAssistant.imageOpen')}>
            <img src={source} alt={t('pages.aiAssistant.visualEvidenceAlt')} loading="lazy" className="max-h-56 w-full object-contain" />
        </Button> : durable && !requested ? <Button type="button" variant="ghost" className="w-full" onClick={() => setRequested(true)}>{t('pages.aiAssistant.imageOpen')}</Button> : <div className="flex h-24 items-center justify-center px-3 text-center text-xs text-muted-foreground">
            {t(durable ? (failed ? 'pages.aiAssistant.imageUnavailable' : 'pages.aiAssistant.imageLoading')
                : frame.status === 'expired' ? 'pages.aiAssistant.visualEvidenceExpired' : 'pages.aiAssistant.visualEvidenceNotRetained')}
        </div>}
        <AssistantFrameTiming frame={frame.frame_observation} />
        <div className="flex items-center justify-between gap-2 border-t p-2 text-xs">
            <div><div>{t(`pages.aiAssistant.visualEvidencePhase.${frame.phase}`)}</div>
                <div className="text-muted-foreground">{new Date(frame.captured_at_unix_ms).toLocaleString()}</div></div>
            {durable && <Button type="button" size="sm" variant="ghost" disabled={deleting} onClick={() => void remove()}>{t('pages.aiAssistant.imageDelete')}</Button>}
        </div>
        <Dialog open={open} onOpenChange={setOpen}><DialogContent className="max-w-[95vw] sm:max-w-[90vw]">
            <DialogTitle>{t('pages.aiAssistant.visualEvidenceAlt')}</DialogTitle>
            {source && <img src={source} alt={t('pages.aiAssistant.visualEvidenceAlt')} className="max-h-[80vh] w-full object-contain" />}
        </DialogContent></Dialog>
    </div>;
}

export function AssistantImages({ sessionId, evidence, messages, renderMessage, renderReasoning, renderToolGroup }: {
    sessionId?: string;
    evidence: AiAssistantVisualEvidence[];
    messages?: AiAssistantMessage[];
    renderMessage?: (message: AiAssistantMessage) => ReactNode;
    renderReasoning?: (message: AiAssistantMessage) => ReactNode;
    renderToolGroup?: (messages: AiAssistantMessage[]) => ReactNode;
}) {
    const { t } = useTranslation();
    const [stored, setStored] = useState<AiAssistantVisualEvidence[]>([]);
    const [deleted, setDeleted] = useState<Set<string>>(new Set());
    const exhausted = useRef(false);
    const [cursor, setCursor] = useState<string | null>(null);
    const [loading, setLoading] = useState(false);
    const [failed, setFailed] = useState(false);
    const [unplacedOpen, setUnplacedOpen] = useState(false);
    const run = sessionId ?? evidence[0]?.conversation_id;
    useEffect(() => {
        setStored([]);
        setDeleted(new Set());
        setCursor(null);
        setUnplacedOpen(false);
        exhausted.current = false;
    }, [run]);
    useEffect(() => {
        if (!run) return;
        const controller = new AbortController();
        void listAssistantImages({ session: run }, { signal: controller.signal }).then((result) => {
            if (controller.signal.aborted) return;
            const frames = result.data ?? [];
            setStored((previous) => [...new Map([...previous, ...frames].map((f) => [f.evidence_id, f])).values()]);
            if (!exhausted.current) setCursor((previous) => previous ?? (frames.length === 32 ? frames.at(-1)!.evidence_id : null));
            setFailed(false);
        }).catch(() => { if (!controller.signal.aborted) setFailed(true); });
        return () => controller.abort();
    }, [run, evidence]);
    // The attachment index is authoritative for durable images. Session frames
    // may still mention a deleted attachment after reopening a conversation.
    const frames = useMemo(() => [...new Map([...evidence.filter((f) => f.content?.kind !== 'artifact'), ...stored].map((f) => [f.evidence_id, f])).values()]
        .filter((f) => f.conversation_id === run && !deleted.has(f.evidence_id))
        .sort((a, b) => a.captured_at_unix_ms - b.captured_at_unix_ms), [evidence, stored, deleted, run]);
    const more = async () => {
        if (!run || !cursor) return;
        setLoading(true);
        try {
            const result = await listAssistantImages({ session: run, before: cursor });
            const next = result.data ?? [];
            setStored((previous) => [...new Map([...previous, ...next].map((f) => [f.evidence_id, f])).values()]);
            exhausted.current = next.length < 32;
            setCursor(next.length === 32 ? next.at(-1)!.evidence_id : null);
            setFailed(false);
        } catch { setFailed(true); } finally { setLoading(false); }
    };
    const renderImages = (items: AiAssistantVisualEvidence[]) => items.length > 0 ? (
        <div data-testid="ai-assistant-visual-evidence" className="grid w-full max-w-full gap-3 sm:max-w-[90%] sm:grid-cols-2">
            {items.map((frame) => <StoredImage key={frame.evidence_id} frame={frame}
                onDelete={(id) => setDeleted((previous) => new Set([...previous, id]))} />)}
        </div>
    ) : null;
    const controls = <>
        {failed && <p className="text-xs text-destructive">{t('pages.aiAssistant.imageUnavailable')}</p>}
        {cursor && <Button type="button" variant="outline" size="sm" disabled={loading} onClick={() => void more()}>{t('pages.aiAssistant.imageMore')}</Button>}
    </>;
    if (messages && renderMessage) {
        // Link to the final record for the call, so its result precedes its images.
        const callAnchors = new Map(messages.flatMap((message, index) => message.toolCallId ? [[message.toolCallId, index] as const] : []));
        const turnAnchors = new Map(messages.flatMap((message, index) => message.turnId &&
            (message.role === 'tool_call' || message.role === 'tool_result') ? [[message.turnId, index] as const] : []));
        messages.forEach((message, index) => {
            if (message.turnId && !turnAnchors.has(message.turnId)) turnAnchors.set(message.turnId, index);
        });
        const anchorFor = (frame: AiAssistantVisualEvidence) => frame.tool_call_id
            ? callAnchors.get(frame.tool_call_id)
            : turnAnchors.get(frame.turn_id);
        const earlier = frames.filter(frame => anchorFor(frame) === undefined);
        const byMessage = new Map<number, AiAssistantVisualEvidence[]>();
        for (const frame of frames) {
            const index = anchorFor(frame);
            if (index !== undefined) byMessage.set(index, [...(byMessage.get(index) ?? []), frame]);
        }
        type TimelineEntry = { key: string; kind: 'body' | 'boundary' | 'activity'; count: number; content: ReactNode };
        const timeline: TimelineEntry[] = [];
        let group: AiAssistantMessage[] = [];
        let groupEnd = -1;
        const flushGroup = () => {
            if (!group.length) return;
            const members = group;
            const end = groupEnd;
            timeline.push({
                key: `tool-group:${members[0].id}`,
                kind: 'activity',
                count: new Set(members.map(item => item.toolCallId).filter(Boolean)).size
                    || members.filter(item => item.role === 'tool_call').length || members.length,
                content: <>{renderToolGroup ? renderToolGroup(members) : members.map(renderMessage)}
                    {renderImages(byMessage.get(end) ?? [])}</>,
            });
            group = [];
            groupEnd = -1;
        };
        messages.forEach((message, index) => {
            if (renderToolGroup && (message.role === 'tool_call' || message.role === 'tool_result')) {
                if (group.length && group[0].turnId !== message.turnId) flushGroup();
                group.push(message);
                groupEnd = index;
                if (byMessage.has(index)) flushGroup();
                return;
            }
            flushGroup();
            if (message.role === 'assistant' && renderReasoning && message.reasoning?.trim()) {
                timeline.push({ key: `reasoning:${message.id}`, kind: 'activity', count: 1,
                    content: renderReasoning(message) });
                if (message.text.trim()) {
                    timeline.push({ key: message.id, kind: 'body', count: 0,
                        content: <>{renderMessage({ ...message, reasoning: null })}{renderImages(byMessage.get(index) ?? [])}</> });
                } else {
                    timeline.push({ key: `reasoning-notices:${message.id}`, kind: 'activity', count: 0,
                        content: <>{renderMessage({ ...message, reasoning: null })}{renderImages(byMessage.get(index) ?? [])}</> });
                }
            } else {
                timeline.push({ key: message.id,
                    kind: message.role === 'assistant' && message.text.trim() ? 'body'
                        : message.role === 'user' ? 'boundary' : 'activity',
                    count: message.role === 'tool_call' ? 1 : 0,
                    content: <>{renderMessage(message)}{renderImages(byMessage.get(index) ?? [])}</> });
            }
        });
        flushGroup();
        const folded: ReactNode[] = [];
        const pending: TimelineEntry[] = [];
        let previousBody = false;
        const flushPending = (closedByBody: boolean) => {
            if (!pending.length) return;
            const count = pending.reduce((sum, entry) => sum + entry.count, 0);
            if (previousBody && closedByBody && count > 1) {
                folded.push(<Disclosure key={`activity:${pending[0].key}`}
                    title={t('pages.aiAssistant.activityGroup', { count })}
                    className="w-full max-w-full rounded-lg border bg-muted/30 px-3 py-2 text-sm sm:max-w-[90%]"
                    summaryClassName="text-muted-foreground">
                    <div className="space-y-3 pt-3">{pending.map(entry => <Fragment key={entry.key}>{entry.content}</Fragment>)}</div>
                </Disclosure>);
            } else {
                folded.push(...pending.map(entry => <Fragment key={entry.key}>{entry.content}</Fragment>));
            }
            pending.length = 0;
        };
        for (const entry of timeline) {
            if (entry.kind === 'body') {
                flushPending(true);
                folded.push(<Fragment key={entry.key}>{entry.content}</Fragment>);
                previousBody = true;
            } else if (entry.kind === 'boundary') {
                flushPending(false);
                folded.push(<Fragment key={entry.key}>{entry.content}</Fragment>);
                previousBody = false;
            } else if (previousBody) pending.push(entry);
            else folded.push(<Fragment key={entry.key}>{entry.content}</Fragment>);
        }
        flushPending(false);
        return <>
            {controls}
            {earlier.length > 0 && <>
                <Button type="button" variant="ghost" size="sm" onClick={() => setUnplacedOpen(true)}>
                    {t('pages.aiAssistant.imageEarlier')} · {earlier.length}
                </Button>
                <Dialog open={unplacedOpen} onOpenChange={setUnplacedOpen}>
                    <DialogContent className="max-h-[85vh] max-w-[95vw] overflow-y-auto sm:max-w-2xl">
                        <DialogTitle>{t('pages.aiAssistant.imageEarlier')}</DialogTitle>
                        {renderImages(earlier)}
                    </DialogContent>
                </Dialog>
            </>}
            {folded}
        </>;
    }
    if (!frames.length && !failed) return null;
    return <div className="space-y-2">{renderImages(frames)}{controls}</div>;
}
