import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getAccessCodeProtection, updateAccessCodeProtection } from '@/services/clients';
import type { AccessCodeProtectionSettings, UpdateAccessCodeProtectionSettings } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

const fields = [
    ['request_window_sec', 'requestWindow', 86400],
    ['user_max_attempts', 'userAttempts', 100000],
    ['network_max_attempts', 'networkAttempts', 100000],
    ['failure_window_sec', 'failureWindow', 86400],
    ['max_failures', 'maxFailures', 100000],
    ['lock_base_sec', 'lockBase', 86400],
    ['lock_max_sec', 'lockMax', 86400],
] as const;
type Key = typeof fields[number][0];
type Draft = Record<Key, string>;
type Result = { success: boolean; data?: AccessCodeProtectionSettings | null };
type Props = {
    loadSettings?: () => Promise<Result>;
    saveSettings?: (data: UpdateAccessCodeProtectionSettings) => Promise<Result>;
};
export function AccessCodeProtectionSettingsCard({ loadSettings = getAccessCodeProtection, saveSettings = updateAccessCodeProtection }: Props) {
    const { t } = useTranslation();
    const [config, setConfig] = useState<AccessCodeProtectionSettings | null>(null);
    const [draft, setDraft] = useState<Draft>(() => Object.fromEntries(fields.map(([key]) => [key, ''])) as Draft);
    const [enabled, setEnabled] = useState(true);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const [saved, setSaved] = useState(false);
    const adopt = (value: AccessCodeProtectionSettings) => {
        setConfig(value);
        setDraft(Object.fromEntries(fields.map(([key]) => [key, value.policy[key] == null ? '' : String(value.policy[key])])) as Draft);
        setEnabled(value.policy.backoff_enabled);
    };
    useEffect(() => {
        let cancelled = false;
        void loadSettings().then(result => {
            if (cancelled) return;
            if (result.success && result.data) adopt(result.data); else setError(true);
        }).catch(() => { if (!cancelled) setError(true); });
        return () => { cancelled = true; };
    }, [loadSettings]);
    const visible = fields.filter(([key]) => key !== 'user_max_attempts' || config?.policy.user_max_attempts != null);
    const valid = visible.every(([key, , max]) => /^\d+$/.test(draft[key]) && Number.isInteger(Number(draft[key])) && Number(draft[key]) >= 1 && Number(draft[key]) <= max)
        && Number(draft.lock_base_sec) <= Number(draft.lock_max_sec);
    const changed = !!config && (enabled !== config.policy.backoff_enabled || visible.some(([key]) => Number(draft[key]) !== config.policy[key]));
    const save = async () => {
        if (!config || !valid) return;
        setBusy(true); setError(false); setSaved(false);
        try {
            const policy = { ...config.policy, backoff_enabled: enabled };
            for (const [key] of visible) policy[key] = Number(draft[key]);
            const result = await saveSettings({ expected_revision: config.revision, policy });
            if (!result.success || !result.data) throw new Error('save failed');
            adopt(result.data); setSaved(true);
        } catch {
            setError(true);
            try { const fresh = await loadSettings(); if (fresh.success && fresh.data) adopt(fresh.data); } catch { /* Keep the draft until a successful reload. */ }
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.accessCodeProtection.title')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.accessCodeProtection.description')}</p>
            <label className="flex items-center justify-between gap-4">
                <span>{t('pages.accessCodeProtection.enabled')}</span>
                <Switch checked={enabled} disabled={!config || busy} onCheckedChange={value => { setEnabled(value); setSaved(false); }} aria-label={t('pages.accessCodeProtection.enabled')} />
            </label>
            <div className="grid gap-4 sm:grid-cols-2">
                {visible.map(([key, label, max]) => <label key={key} className="block space-y-2">
                    <span>{t(`pages.accessCodeProtection.${label}`)}</span>
                    <Input type="number" min={1} max={max} step={1} value={draft[key]} disabled={!config || busy}
                        aria-label={t(`pages.accessCodeProtection.${label}`)}
                        onChange={event => { setDraft(current => ({ ...current, [key]: event.target.value })); setSaved(false); }} />
                </label>)}
            </div>
            {config && !valid && <p role="alert">{t('pages.accessCodeProtection.invalid')}</p>}
            {error && <p role="alert">{t('pages.accessCodeProtection.error')}</p>}
            {saved && <p role="status">{t('pages.accessCodeProtection.saved')}</p>}
            <Button onClick={() => void save()} disabled={!config || busy || !valid || !changed}>{t('common.save')}</Button>
        </CardContent>
    </Card>;
}
