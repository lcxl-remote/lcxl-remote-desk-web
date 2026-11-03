import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getDeviceCodeSettings, updateDeviceCodeSettings } from '@/services/clients';
import type { DeviceCodeSettings, UpdateDeviceCodeSettings } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

type Result = { success: boolean; data?: DeviceCodeSettings | null };
type Props = {
    loadSettings?: () => Promise<Result>;
    saveSettings?: (data: UpdateDeviceCodeSettings) => Promise<Result>;
};
export function DeviceCodeSettingsCard({ loadSettings = getDeviceCodeSettings, saveSettings = updateDeviceCodeSettings }: Props) {
    const { t } = useTranslation();
    const [config, setConfig] = useState<DeviceCodeSettings | null>(null);
    const [length, setLength] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const [saved, setSaved] = useState(false);
    const adopt = (value: DeviceCodeSettings) => { setConfig(value); setLength(String(value.length)); };
    useEffect(() => {
        let cancelled = false;
        void loadSettings().then(result => {
            if (cancelled) return;
            if (result.success && result.data) adopt(result.data); else setError(true);
        }).catch(() => { if (!cancelled) setError(true); });
        return () => { cancelled = true; };
    }, [loadSettings]);
    const value = Number(length);
    const valid = /^\d+$/.test(length) && Number.isInteger(value) && value >= 6 && value <= 64;
    const save = async () => {
        if (!config || !valid) return;
        setBusy(true); setError(false); setSaved(false);
        try {
            const result = await saveSettings({ expected_revision: config.revision, length: value });
            if (!result.success || !result.data) throw new Error('save failed');
            adopt(result.data); setSaved(true);
        } catch {
            setError(true);
            try { const fresh = await loadSettings(); if (fresh.success && fresh.data) adopt(fresh.data); } catch { /* Keep the draft if reloading fails. */ }
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.deviceCodeSettings.title')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.deviceCodeSettings.description')}</p>
            <label className="block space-y-2">
                <span>{t('pages.deviceCodeSettings.length')}</span>
                <Input type="number" min={6} max={64} step={1} value={length} disabled={!config || busy}
                    aria-label={t('pages.deviceCodeSettings.length')}
                    onChange={event => { setLength(event.target.value); setSaved(false); }} />
            </label>
            {config && !valid && <p role="alert">{t('pages.deviceCodeSettings.invalid')}</p>}
            {error && <p role="alert">{t('pages.deviceCodeSettings.error')}</p>}
            {saved && <p role="status">{t('pages.deviceCodeSettings.saved')}</p>}
            <Button onClick={() => void save()} disabled={!config || busy || !valid || value === config.length}>{t('common.save')}</Button>
        </CardContent>
    </Card>;
}
