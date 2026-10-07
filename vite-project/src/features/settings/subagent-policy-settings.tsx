import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import type { SubAgentLimits, SubAgentPolicy } from '@/services/types';

const path = '/api/admin/system/subagent-policy';
const fields = [
    { key: 'maxUnfinishedPerRoot', label: 'subagentPolicyUnfinished', maximum: 32 },
] as const;

function validLimits(limits: SubAgentLimits | undefined): boolean {
    return !!limits && fields.every(({ key, maximum }) =>
        Number.isSafeInteger(limits[key]) && limits[key] >= 1 && limits[key] <= maximum);
}

function validPolicy(value: SubAgentPolicy | undefined): value is SubAgentPolicy {
    return !!value && value.schemaVersion === 1 && Number.isSafeInteger(value.revision)
        && value.revision >= 0 && validLimits(value.limits);
}

export default function SubAgentPolicySettings() {
    const { t } = useTranslation();
    const [current, setCurrent] = useState<SubAgentPolicy | null>(null);
    const [input, setInput] = useState<Record<keyof SubAgentLimits, string> | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [saved, setSaved] = useState(false);
    const adopt = (policy: SubAgentPolicy) => {
        setCurrent(policy);
        setInput({ maxUnfinishedPerRoot: String(policy.limits.maxUnfinishedPerRoot) });
    };
    const reload = async () => {
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch(path, { credentials: 'include', cache: 'no-store' });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? t('pages.aiAssistant.subagentPolicyLoadFailed'));
            adopt(body.data);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : t('pages.aiAssistant.subagentPolicyLoadFailed'));
        } finally { setBusy(false); }
    };
    useEffect(() => { void reload(); }, []);
    const next: SubAgentLimits | undefined = input ? {
        maxUnfinishedPerRoot: Number(input.maxUnfinishedPerRoot),
    } : undefined;
    const valid = validLimits(next);
    const dirty = !!current && !!next && fields.some(({ key }) => current.limits[key] !== next[key]);
    const save = async () => {
        if (!current || !next || !valid || !dirty || busy) return;
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch(path, {
                method: 'PUT', credentials: 'include', cache: 'no-store',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({ expectedRevision: current.revision, limits: next }),
            });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? t('pages.aiAssistant.subagentPolicySaveFailed'));
            adopt(body.data); setSaved(true);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : t('pages.aiAssistant.subagentPolicySaveFailed'));
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.aiAssistant.subagentPolicyTitle')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.subagentPolicyDescription')}</p>
            {fields.map(({ key, label, maximum }) => <div key={key} className="space-y-1">
                <div className="flex flex-wrap items-center gap-3">
                    <label htmlFor={key} className="min-w-56">{t(`pages.aiAssistant.${label}`)}</label>
                    <Input id={key} className="w-32" type="number" min={1} max={maximum} step={1}
                        value={input?.[key] ?? ''} disabled={!current || busy}
                        onChange={(event) => { setSaved(false); setInput(previous => previous && ({ ...previous, [key]: event.target.value })); }} />
                </div>
                <p className="text-sm text-muted-foreground">{t(`pages.aiAssistant.${label}Description`)}</p>
            </div>)}
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.subagentPolicyBudget')}</p>
            {error && <p role="alert" className="text-destructive">{error}</p>}
            {current && !valid && <p role="alert" className="text-destructive">{t('pages.aiAssistant.subagentPolicyInvalid')}</p>}
            {saved && <p role="status">{t('pages.aiAssistant.subagentPolicySaved')}</p>}
            <div className="flex gap-2">
                <Button onClick={() => void save()} disabled={!current || busy || !valid || !dirty}>{t('pages.aiAssistant.goalBudgetSave')}</Button>
                <Button variant="outline" onClick={() => void reload()} disabled={busy}>{t('pages.aiAssistant.goalBudgetReload')}</Button>
            </div>
        </CardContent>
    </Card>;
}
