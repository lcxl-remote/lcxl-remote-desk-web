import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getContextManagement, updateContextManagement } from '@/services/clients';
import type { ContextManagementDto } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Switch } from '@/components/ui/switch';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

export function ContextManagementSettings() {
    const { t } = useTranslation();
    const [config, setConfig] = useState<ContextManagementDto | null>(null);
    const [enabled, setEnabled] = useState(true);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const [saved, setSaved] = useState(false);
    useEffect(() => {
        let cancelled = false;
        void getContextManagement().then(result => {
            if (cancelled) return;
            if (!result.success || !result.data) { setError(true); return; }
            setConfig(result.data);
            setEnabled(result.data.strategy === 'checkpoint_summary');
        }).catch(() => { if (!cancelled) setError(true); });
        return () => { cancelled = true; };
    }, []);
    const save = async () => {
        if (!config) return;
        setBusy(true); setError(false); setSaved(false);
        try {
            const result = await updateContextManagement({ expectedRevision: config.revision,
                strategy: enabled ? 'checkpoint_summary' : 'window' });
            if (!result.success || !result.data) {
                const fresh = await getContextManagement();
                if (fresh.success && fresh.data) { setConfig(fresh.data); setEnabled(fresh.data.strategy === 'checkpoint_summary'); }
                setError(true); return;
            }
            setConfig(result.data); setEnabled(result.data.strategy === 'checkpoint_summary'); setSaved(true);
        } catch { setError(true); } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.contextManagement.title')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.contextManagement.description')}</p>
            <label className="flex items-center justify-between gap-4">
                {t('pages.contextManagement.enabled')}
                <Switch checked={enabled} onCheckedChange={value => { setEnabled(value); setSaved(false); }} disabled={!config || busy} aria-label={t('pages.contextManagement.enabled')} />
            </label>
            <p className="text-sm">{t(enabled ? 'pages.contextManagement.summary' : 'pages.contextManagement.window')}</p>
            <p className="text-sm text-amber-700 dark:text-amber-300">{t('pages.contextManagement.warning')}</p>
            {error && <p role="alert">{t('pages.contextManagement.error')}</p>}
            {saved && <p role="status">{t('pages.contextManagement.saved')}</p>}
            <Button onClick={() => void save()} disabled={!config || busy || enabled === (config.strategy === 'checkpoint_summary')}>{t('pages.contextManagement.save')}</Button>
        </CardContent>
    </Card>;
}
