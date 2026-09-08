import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { RehearsalView } from '@/services/types';
import { ScheduleClient } from '../schedules/client';
import { useDeskSignaling } from './use-desk-signaling';

export function DeviceAssistantRehearsalGate({ rehearsalId, deviceId, children }: {
    rehearsalId: string; deviceId: string; children: (rehearsal: RehearsalView) => ReactNode;
}) {
    const { t } = useTranslation();
    const { isConnected, subscribe, sendTracked, cancelQueued } = useDeskSignaling();
    const client = useMemo(() => new ScheduleClient(sendTracked, cancelQueued), [sendTracked, cancelQueued]);
    const [row, setRow] = useState<{ rehearsal: RehearsalView; client: ScheduleClient; refresh: number } | null>(null);
    const [failed, setFailed] = useState(false);
    const [refresh, setRefresh] = useState(0);
    useEffect(() => {
        const unsubscribe = subscribe(client.receive);
        return () => { unsubscribe(); client.close(); };
    }, [client, subscribe]);
    useEffect(() => {
        let active = true;
        setRow(null); setFailed(false);
        if (!isConnected) { client.close(); return () => { active = false; }; }
        if (!rehearsalId || rehearsalId.length > 256) { setFailed(true); return () => { active = false; }; }
        void client.request({ operation: 'get_rehearsal', rehearsal_id: rehearsalId }).then(response => {
            if (!active) return;
            if (response.result !== 'rehearsal' || response.rehearsal.rehearsal_id !== rehearsalId
                || response.rehearsal.target_device_id !== deviceId
                || !response.rehearsal.client_conversation_id.startsWith('rehearsal_')
                || response.rehearsal.initial_message_id !== `rehearsal:${rehearsalId}:input`) {
                setFailed(true); return;
            }
            setRow({ rehearsal: response.rehearsal, client, refresh });
        }).catch(() => { if (active) setFailed(true); });
        return () => { active = false; };
    }, [client, rehearsalId, deviceId, isConnected, refresh]);
    if (row && row.client === client && row.refresh === refresh && isConnected
        && row.rehearsal.rehearsal_id === rehearsalId && row.rehearsal.target_device_id === deviceId) return children(row.rehearsal);
    return <div className="space-y-3">
        <p role={failed ? 'alert' : 'status'}>{t(!isConnected ? 'schedules.connecting' : failed ? 'schedules.rehearsal.unavailable' : 'schedules.loading')}</p>
        {failed && <Button variant="outline" disabled={!isConnected} onClick={() => setRefresh(value => value + 1)}>{t('schedules.refresh')}</Button>}
    </div>;
}
