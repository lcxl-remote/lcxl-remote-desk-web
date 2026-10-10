import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getTerminalCompletion, updateTerminalCompletion } from '@/services/clients';
import type { TerminalCompletionDto } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { parseOutputTokens } from '@/lib/output-token-settings';

export function TerminalCompletionSettings() {
    const { t } = useTranslation();
    const [config, setConfig] = useState<TerminalCompletionDto | null>(null);
    const [tokens, setTokens] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const [saved, setSaved] = useState(false);
    const adopt = (value: TerminalCompletionDto) => { setConfig(value); setTokens(String(value.maxOutputTokens)); };
    useEffect(() => {
        let cancelled = false;
        void getTerminalCompletion().then(result => {
            if (cancelled) return;
            if (result.success && result.data) adopt(result.data); else setError(true);
        }).catch(() => { if (!cancelled) setError(true); });
        return () => { cancelled = true; };
    }, []);
    const parsed = parseOutputTokens(tokens);
    const save = async () => {
        if (!config || parsed === null) return;
        setBusy(true); setError(false); setSaved(false);
        try {
            const result = await updateTerminalCompletion({ expectedRevision: config.revision, maxOutputTokens: parsed });
            if (!result.success || !result.data) throw new Error('save failed');
            const fresh = await getTerminalCompletion();
            if (!fresh.success || !fresh.data) throw new Error('reload failed');
            adopt(fresh.data); setSaved(true);
        } catch {
            setError(true);
            try { const fresh = await getTerminalCompletion(); if (fresh.success && fresh.data) adopt(fresh.data); } catch { /* Keep the draft if reloading fails. */ }
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.terminalCompletion.title')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.terminalCompletion.description')}</p>
            <label className="block space-y-2">
                <span>{t('pages.terminalCompletion.outputTokens')}</span>
                <Input type="number" min={1} max={4294967295} step={1} value={tokens} disabled={!config || busy} onChange={event => { setTokens(event.target.value); setSaved(false); }} aria-label={t('pages.terminalCompletion.outputTokens')} />
            </label>
            {config && parsed === null && <p role="alert">{t('pages.outputTokens.invalid')}</p>}
            {error && <p role="alert">{t('pages.terminalCompletion.error')}</p>}
            {saved && <p role="status">{t('pages.terminalCompletion.saved')}</p>}
            <Button onClick={() => void save()} disabled={!config || busy || parsed === null || parsed === config.maxOutputTokens}>{t('pages.terminalCompletion.save')}</Button>
        </CardContent>
    </Card>;
}
