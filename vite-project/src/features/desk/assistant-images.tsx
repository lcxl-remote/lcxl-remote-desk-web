import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import type { DeviceAssistantMessage } from './use-device-assistant-chat';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog';
import { deleteAssistantImage, getAssistantImage, listAssistantImages } from '@/services/clients';
import type { DeviceAssistantVisualEvidence } from './device-assistant-event';

function StoredImage({ frame, onDelete }: { frame: DeviceAssistantVisualEvidence; onDelete: (id: string) => void }) {
    const { t } = useTranslation();
    const [url, setUrl] = useState<string | null>(null);
    const [failed, setFailed] = useState(false);
    const [open, setOpen] = useState(false);
    const [deleting, setDeleting] = useState(false);
    // Only an explicit artifact reference denotes durable storage.
    const durable = frame.status === 'available' && frame.content?.kind === 'artifact' && frame.content.artifact_id === frame.evidence_id;
    useEffect(() => {
        if (!durable) return;
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
    }, [durable, frame.conversation_id, frame.evidence_id]);
    const source = url ?? (!durable ? frame.preview_data_url : null);
    const remove = async () => {
        if (!window.confirm(t('pages.deviceAssistant.imageDeleteConfirm'))) return;
        setDeleting(true);
        try {
            await deleteAssistantImage({ session: frame.conversation_id, attachment: frame.evidence_id });
            onDelete(frame.evidence_id);
        } catch { setFailed(true); } finally { setDeleting(false); }
    };
    return <div className="overflow-hidden rounded-md border bg-muted/30">
        {source ? <button type="button" className="block w-full" onClick={() => setOpen(true)} aria-label={t('pages.deviceAssistant.imageOpen')}>
            <img src={source} alt={t('pages.deviceAssistant.visualEvidenceAlt')} loading="lazy" className="max-h-56 w-full object-contain" />
        </button> : <div className="flex h-24 items-center justify-center px-3 text-center text-xs text-muted-foreground">
            {t(durable ? (failed ? 'pages.deviceAssistant.imageUnavailable' : 'pages.deviceAssistant.imageLoading')
                : frame.status === 'expired' ? 'pages.deviceAssistant.visualEvidenceExpired' : 'pages.deviceAssistant.visualEvidenceNotRetained')}
        </div>}
        <div className="flex items-center justify-between gap-2 border-t p-2 text-xs">
            <div><div>{t(`pages.deviceAssistant.visualEvidencePhase.${frame.phase}`)}</div>
                <div className="text-muted-foreground">{new Date(frame.captured_at_unix_ms).toLocaleString()}</div></div>
            {durable && <Button type="button" size="sm" variant="ghost" disabled={deleting} onClick={() => void remove()}>{t('pages.deviceAssistant.imageDelete')}</Button>}
        </div>
        <Dialog open={open} onOpenChange={setOpen}><DialogContent className="max-w-[95vw] sm:max-w-[90vw]">
            <DialogTitle>{t('pages.deviceAssistant.visualEvidenceAlt')}</DialogTitle>
            {source && <img src={source} alt={t('pages.deviceAssistant.visualEvidenceAlt')} className="max-h-[80vh] w-full object-contain" />}
        </DialogContent></Dialog>
    </div>;
}

export function AssistantImages({ sessionId, evidence, messages, renderMessage }: {
    sessionId?: string;
    evidence: DeviceAssistantVisualEvidence[];
    messages?: DeviceAssistantMessage[];
    renderMessage?: (message: DeviceAssistantMessage) => ReactNode;
}) {
    const { t } = useTranslation();
    const [stored, setStored] = useState<DeviceAssistantVisualEvidence[]>([]);
    const [deleted, setDeleted] = useState<Set<string>>(new Set());
    const exhausted = useRef(false);
    const [cursor, setCursor] = useState<string | null>(null);
    const [loading, setLoading] = useState(false);
    const [failed, setFailed] = useState(false);
    const run = sessionId ?? evidence[0]?.conversation_id;
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
        .filter((f) => !deleted.has(f.evidence_id)).sort((a, b) => a.captured_at_unix_ms - b.captured_at_unix_ms), [evidence, stored, deleted]);
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
    const renderImages = (items: DeviceAssistantVisualEvidence[]) => items.length > 0 ? (
        <div data-testid="device-assistant-visual-evidence" className="grid max-w-[90%] gap-3 sm:grid-cols-2">
            {items.map((frame) => <StoredImage key={frame.evidence_id} frame={frame}
                onDelete={(id) => setDeleted((previous) => new Set([...previous, id]))} />)}
        </div>
    ) : null;
    const controls = <>
        {failed && <p className="text-xs text-destructive">{t('pages.deviceAssistant.imageUnavailable')}</p>}
        {cursor && <Button type="button" variant="outline" size="sm" disabled={loading} onClick={() => void more()}>{t('pages.deviceAssistant.imageMore')}</Button>}
    </>;
    if (messages && renderMessage) {
        // Link to the final record for the call, so its result precedes its images.
        const anchors = new Map(messages.flatMap((message, index) => message.toolCallId ? [[message.toolCallId, index] as const] : []));
        const earlier = frames.filter(frame => !anchors.has(frame.tool_call_id));
        const byMessage = new Map<number, DeviceAssistantVisualEvidence[]>();
        for (const frame of frames) {
            const index = anchors.get(frame.tool_call_id);
            if (index !== undefined) byMessage.set(index, [...(byMessage.get(index) ?? []), frame]);
        }
        return <>
            {controls}
            {earlier.length > 0 && <div className="space-y-2">
                <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.imageEarlier')}</p>
                {renderImages(earlier)}
            </div>}
            {messages.map((message, index) => <Fragment key={message.id}>
                {renderMessage(message)}
                {renderImages(byMessage.get(index) ?? [])}
            </Fragment>)}
        </>;
    }
    if (!frames.length && !failed) return null;
    return <div className="space-y-2">{renderImages(frames)}{controls}</div>;
}
