import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ResumeConversationSource } from '@/services/types';
import { Button } from '@/components/ui/button';
import { ScheduleClient } from './client';
import type { ScheduleDevice } from './page';

export function ResumeSourcePicker({ client, devices, connected, onSelect }: {
    client: ScheduleClient; devices: ScheduleDevice[]; connected: boolean;
    onSelect: (source: ResumeConversationSource) => void;
}) {
    const { t } = useTranslation();
    const [device, setDevice] = useState(devices[0]?.id ?? '');
    const [sources, setSources] = useState<ResumeConversationSource[]>([]);
    const [offset, setOffset] = useState<number | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const generation = useRef(0);
    const pending = useRef(false);
    const load = async (next = 0) => {
        if (!connected || !device || pending.current) return;
        const epoch = ++generation.current;
        pending.current = true; setBusy(true); setError(false);
        try {
            const result = await client.request({ operation: 'list_resume_sources', target_device_id: device, offset: next, limit: 25 });
            if (epoch !== generation.current) return;
            if (result.result !== 'resume_sources' || result.sources.some(source => source.target_device_id !== device
                || !Number.isSafeInteger(source.requirement_revision) || source.requirement_revision < 1)) throw new Error('Invalid conversation choices');
            setSources(previous => {
                const items = next ? [...previous, ...result.sources] : result.sources;
                return [...new Map(items.map(source => [source.client_conversation_id, source])).values()];
            });
            setOffset(result.next_offset ?? null);
        } catch { if (epoch === generation.current) setError(true); }
        finally { if (epoch === generation.current) { pending.current = false; setBusy(false); } }
    };
    useEffect(() => {
        ++generation.current; pending.current = false; setSources([]); setOffset(null); setBusy(false);
        void load();
        return () => { ++generation.current; pending.current = false; };
    }, [client, device, connected]);
    return <div className="space-y-3">
        <p>{t('schedules.chooseConversationNote')}</p>
        <label>{t('schedules.device')}
            <select className="block w-full rounded border bg-background p-2" value={device} disabled={!connected || busy}
                onChange={event => setDevice(event.target.value)}>
                {devices.map(item => <option key={item.id} value={item.id}>{item.name}</option>)}
            </select>
        </label>
        {error && <p role="alert">{t('schedules.requestFailed')}</p>}
        {!busy && !sources.length && <p>{t('schedules.noConversations')}</p>}
        {sources.map(source => <Button key={source.client_conversation_id} variant="outline" className="h-auto w-full whitespace-pre-wrap text-left"
            disabled={!connected || busy} onClick={() => onSelect(source)}>{source.title || source.client_conversation_id}</Button>)}
        <Button variant="outline" disabled={!connected || busy} onClick={() => void load()}>{t('schedules.policy.reload')}</Button>
        {offset !== null && <Button variant="outline" disabled={!connected || busy} onClick={() => void load(offset)}>{t('schedules.more')}</Button>}
    </div>;
}
