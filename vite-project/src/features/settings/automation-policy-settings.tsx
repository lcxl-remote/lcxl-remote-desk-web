import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import type { AutomationPolicy } from '@/services/types';

const path = '/api/admin/system/automation-policy';
const minimum = 1;
const maximum = 50;

function validLimit(value: number): boolean {
    return Number.isSafeInteger(value) && value >= minimum && value <= maximum;
}

function validPolicy(value: AutomationPolicy | undefined): value is AutomationPolicy {
    return !!value && value.schemaVersion === 1 && Number.isSafeInteger(value.revision)
        && value.revision >= 0 && validLimit(value.maxAutoTurnsPerChain);
}

export default function AutomationPolicySettings() {
    const { t } = useTranslation();
    const [current, setCurrent] = useState<AutomationPolicy | null>(null);
    const [input, setInput] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [saved, setSaved] = useState(false);
    const adopt = (policy: AutomationPolicy) => {
        setCurrent(policy);
        setInput(String(policy.maxAutoTurnsPerChain));
    };
    const reload = async () => {
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch(path, { credentials: 'include', cache: 'no-store' });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? t('pages.aiAssistant.automationPolicyLoadFailed'));
            adopt(body.data);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : t('pages.aiAssistant.automationPolicyLoadFailed'));
        } finally { setBusy(false); }
    };
    useEffect(() => { void reload(); }, []);
    const next = Number(input);
    const valid = validLimit(next);
    const dirty = !!current && current.maxAutoTurnsPerChain !== next;
    const save = async () => {
        if (!current || !valid || !dirty || busy) return;
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch(path, {
                method: 'PUT', credentials: 'include', cache: 'no-store',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({ expectedRevision: current.revision, maxAutoTurnsPerChain: next }),
            });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? t('pages.aiAssistant.automationPolicySaveFailed'));
            adopt(body.data); setSaved(true);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : t('pages.aiAssistant.automationPolicySaveFailed'));
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.aiAssistant.automationPolicyTitle')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.automationPolicyDescription')}</p>
            <div className="flex flex-wrap items-center gap-3">
                <label htmlFor="max-auto-turns-per-chain" className="min-w-56">{t('pages.aiAssistant.automationPolicyMaxTurns')}</label>
                <Input id="max-auto-turns-per-chain" className="w-32" type="number" min={minimum} max={maximum} step={1}
                    value={input} disabled={!current || busy}
                    onChange={(event) => { setSaved(false); setInput(event.target.value); }} />
            </div>
            {error && <p role="alert" className="text-destructive">{error}</p>}
            {current && !valid && <p role="alert" className="text-destructive">{t('pages.aiAssistant.automationPolicyInvalid')}</p>}
            {saved && <p role="status">{t('pages.aiAssistant.automationPolicySaved')}</p>}
            <div className="flex gap-2">
                <Button onClick={() => void save()} disabled={!current || busy || !valid || !dirty}>{t('pages.aiAssistant.goalBudgetSave')}</Button>
                <Button variant="outline" onClick={() => void reload()} disabled={busy}>{t('pages.aiAssistant.goalBudgetReload')}</Button>
            </div>
        </CardContent>
    </Card>;
}
