import { useEffect, useMemo, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import type { MetricsSettings } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { isMetricsRevisionConflict } from './api';
import type { MetricsApi } from './dashboard';

function MetricsSettingsForm({ settings, save, reload }: { settings: MetricsSettings; save: (value: MetricsSettings) => Promise<MetricsSettings>; reload: () => void }) {
    const { t } = useTranslation();
    const [draft, setDraft] = useState(settings);
    const [pending, setPending] = useState(false);
    const [failed, setFailed] = useState<'failed' | 'conflict' | null>(null);
    const fields = {
        detail_days: [1, 90], five_minute_days: [1, 30], hourly_days: [1, 365],
        mutable_days: [1, Math.min(draft.detail_days, draft.hourly_days)],
        detail_row_budget: [1000, 5000000], event_row_budget: [1000, 1000000],
        compact_row_budget: [draft.detail_row_budget, 5000000], rollup_row_budget: [1000, 1000000], series_per_bucket: [16, 1024],
    };
    return <form className="space-y-4" onSubmit={async (event) => {
        event.preventDefault(); setPending(true); setFailed(null);
        try { setDraft(await save(draft)); } catch (error) { setFailed(isMetricsRevisionConflict(error) ? 'conflict' : 'failed'); } finally { setPending(false); }
    }}>
        <Label className="flex gap-2"><input type="checkbox" disabled={pending} checked={draft.enabled} onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}/>{t('modelMetrics.enabled')}</Label>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {Object.entries(fields).map(([field, [min, max]]) => <Label key={field} className="space-y-2">{t(`modelMetrics.setting.${field}`)}<Input type="number" aria-label={t(`modelMetrics.setting.${field}`)} min={min} max={max} step="1" required disabled={pending} value={draft[field as keyof typeof fields]} onChange={(event) => setDraft({ ...draft, [field]: Number(event.target.value) })}/></Label>)}
            <Label className="space-y-2">{t('modelMetrics.setting.storage_budget_bytes')}<Input aria-label={t('modelMetrics.setting.storage_budget_bytes')} inputMode="numeric" pattern="[1-9][0-9]*" required disabled={pending} value={draft.storage_budget_bytes} onChange={(event) => setDraft({ ...draft, storage_budget_bytes: event.target.value })}/></Label>
        </div>
        <p className="text-sm text-muted-foreground">{t('modelMetrics.settingsHint', { revision: draft.revision })}</p>
        <p className="text-sm text-muted-foreground">{t('modelMetrics.settingBoundsHint')}</p>
        {failed && <p role="alert">{t(failed === 'conflict' ? 'modelMetrics.settingsConflict' : 'modelMetrics.saveFailed')}</p>}
        <div className="flex gap-3"><Button disabled={pending || failed === 'conflict'} type="submit">{t('modelMetrics.save')}</Button>{failed && <Button disabled={pending} type="button" variant="outline" onClick={reload}>{t('modelMetrics.reloadSettings')}</Button>}</div>
    </form>;
}

export function MetricsCollectionSettings({ api, identity, allowed }: { api: MetricsApi; identity: string; allowed: boolean }) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const [mountIdentity] = useState(() => globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`);
    const [invalidAccess, setInvalidAccess] = useState(false);
    const key = useMemo(() => ['model-metrics-settings', location.origin, identity, mountIdentity] as const, [identity, mountIdentity]);
    useEffect(() => {
        setInvalidAccess(false);
        return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); };
    }, [client, key]);
    const settings = useQuery({ queryKey: key, queryFn: ({ signal }) => api.settings(signal), enabled: allowed && !invalidAccess,
        retry: false, gcTime: 0, refetchInterval: false, refetchOnWindowFocus: false });
    const denied = api.accessError(settings.error);
    useEffect(() => {
        if (denied) { setInvalidAccess(true); void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }
    }, [denied, client, key]);
    if (!allowed || invalidAccess || denied) return <p role="alert" className="p-6">{t('modelMetrics.noAccess')}</p>;
    return <div className="p-4 md:p-6 space-y-6 max-w-6xl mx-auto">
        <div><h1 className="text-2xl font-semibold">{t('modelMetrics.settingsTitle')}</h1><p className="text-muted-foreground">{t('modelMetrics.settingsDescription')}</p></div>
        <Card><CardHeader><CardTitle>{t('modelMetrics.settingsTitle')}</CardTitle><CardDescription>{t('modelMetrics.coverageHint')}</CardDescription></CardHeader><CardContent>
            {settings.isLoading && <p>{t('modelMetrics.loading')}</p>}
            {settings.error && <p role="alert">{t('modelMetrics.queryFailed')}</p>}
            {settings.data && !settings.error && <MetricsSettingsForm key={`${identity}-${settings.data.revision}`} settings={settings.data} reload={() => void settings.refetch()} save={async (draft) => {
                try {
                    const value = await api.save(draft);
                    client.setQueryData(key, value);
                    void client.invalidateQueries({ queryKey: ['model-metrics', location.origin, identity] });
                    return value;
                } catch (error) { if (api.accessError(error)) setInvalidAccess(true); throw error; }
            }}/>}
        </CardContent></Card>
    </div>;
}
