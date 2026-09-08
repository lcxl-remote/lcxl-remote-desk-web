import { RunUnknownOutcome } from './run-unknown-outcome';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RunDirectories } from './run-directories';
import { RunPermissions } from './run-permissions';
import type { ScheduleClient } from './client';
import { Button } from '@/components/ui/button';
import { deskErrorCodeEnum, type DeviceAssistantSessionSnapshotDto, type SnapshotMessageDto } from '@/services/types';

export function RunResult({ scheduleId, runId, connected, onBack, client, connectionIds }: { client?: Pick<ScheduleClient, 'request'>; connectionIds?: Record<string, string>; scheduleId: string; runId: string; connected: boolean; onBack: () => void }) {
    const { t } = useTranslation();
    const [messages, setMessages] = useState<SnapshotMessageDto[]>([]);
    const [snapshot, setSnapshot] = useState<DeviceAssistantSessionSnapshotDto | null>(null);
    const [cursor, setCursor] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [failed, setFailed] = useState(false);
    const epoch = useRef(0);
    const abort = useRef<AbortController | null>(null);
    const watermark = useRef<{ session: string; seq: number } | null>(null);
    const load = useCallback(async (before: string | null = null) => {
        const current = ++epoch.current;
        abort.current?.abort();
        const controller = new AbortController(); abort.current = controller;
        setBusy(true); setFailed(false);
        try {
            const params = new URLSearchParams({ scheduled_task: scheduleId, scheduled_run: runId, message_limit: '100' });
            if (before) params.set('message_before', before);
            const response = await fetch(`/api/my/device-assistant-session?${params}`, { credentials: 'include', headers: { Accept: 'application/json' }, signal: controller.signal });
            if (!response.ok) throw new Error('snapshot unavailable');
            const body = await response.json();
            if (current !== epoch.current) return;
            const snapshot = body?.data as DeviceAssistantSessionSnapshotDto | undefined;
            if (body?.code !== deskErrorCodeEnum.SUCCESS || !snapshot || !Array.isArray(snapshot.messages)
                || (before && (watermark.current?.session !== snapshot.sessionId || watermark.current?.seq !== snapshot.seq))) throw new Error('snapshot unavailable');
            setSnapshot(snapshot);
            watermark.current = { session: snapshot.sessionId, seq: snapshot.seq };
            setMessages(previous => before ? [...snapshot.messages, ...previous.filter(item => !snapshot.messages.some(older => older.id === item.id))] : snapshot.messages);
            setCursor(snapshot.messagePage.hasMore ? snapshot.messagePage.nextBeforeMessageId ?? null : null);
        } catch {
            if (current === epoch.current) { setFailed(true); setSnapshot(null); setMessages([]); setCursor(null); watermark.current = null; }
        } finally { if (current === epoch.current) setBusy(false); }
    }, [scheduleId, runId]);
    useEffect(() => {
        setSnapshot(null); setMessages([]); setCursor(null); setFailed(false); setBusy(false); watermark.current = null;
        if (connected) void load();
        return () => { ++epoch.current; abort.current?.abort(); };
    }, [connected, load]);
    return <div className="space-y-3" aria-busy={busy}>
        <div className="flex gap-2"><Button variant="outline" onClick={onBack}>{t('schedules.result.back')}</Button><Button variant="outline" disabled={!connected || busy} onClick={() => void load()}>{t('schedules.refresh')}</Button></div>
        <p className="text-sm text-muted-foreground">{t('schedules.result.note')}</p>
        {!connected && <p role="status">{t('schedules.connecting')}</p>}
        {busy && <p role="status">{t('schedules.loading')}</p>}
        {failed && <p role="alert">{t('schedules.result.unavailable')}</p>}
        {connected && !busy && !failed && !messages.length && <p>{t('schedules.result.empty')}</p>}
        {cursor && <Button variant="outline" disabled={!connected || busy} onClick={() => void load(cursor)}>{t('schedules.result.older')}</Button>}
        {client && snapshot && Array.isArray(snapshot.permissionRequests) && snapshot.permissionRequests.length > 0 &&
            <RunPermissions key={`${scheduleId}:${runId}:${snapshot.sessionId}`} client={client} scheduleId={scheduleId} runId={runId}
                snapshot={snapshot} connectionIds={connectionIds} connected={connected} loading={busy} onReload={load} />}
        {client && snapshot?.fileScope?.directories?.length ? <RunDirectories
            key={`${scheduleId}:${runId}:${snapshot.sessionId}:directories`} client={client}
            scheduleId={scheduleId} runId={runId} snapshot={snapshot} connected={connected}
            loading={busy} onReload={load} /> : null}
        {client && snapshot?.unresolvedOutcome && <RunUnknownOutcome
            key={`${scheduleId}:${runId}:${snapshot.sessionId}:unknown`} client={client} scheduleId={scheduleId}
            runId={runId} snapshot={snapshot} connected={connected}
            loading={busy} onReload={load} />}
        {messages.map(message => <article key={message.id} className="space-y-2 rounded-lg border p-3">
            <p className="text-sm font-medium">{t(['user', 'assistant', 'tool'].includes(message.role) ? `schedules.result.role.${message.role}` : 'schedules.result.role.other')}
                {message.turnId === `${runId}-turn` && <span className="ml-2">{t('schedules.result.thisRun')}</span>}</p>
            {message.text && <div className="whitespace-pre-wrap break-words text-sm">{message.text}</div>}
            {message.toolCalls?.map(call => <details key={call.id}><summary>{call.name}</summary><pre className="overflow-x-auto whitespace-pre-wrap text-xs">{call.argumentsJson}</pre></details>)}
        </article>)}
    </div>;
}
