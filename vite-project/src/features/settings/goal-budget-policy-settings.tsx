import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';

export type GoalBudgetLimits = {
    activeTimeMs: number | null;
    deadlineMs: number | null;
    modelTokens: number | null;
    modelCalls: number | null;
    toolCalls: number | null;
    slices: number | null;
    stalledSlices: number | null;
};

export type GoalBudgetPolicy = { schemaVersion: number; revision: number; limits: GoalBudgetLimits };

const fields = [
    { key: 'activeTimeMs', label: 'goalBudgetActiveHours', scale: 3_600_000, maximum: 24, defaultValue: 2 },
    { key: 'deadlineMs', label: 'goalBudgetDeadlineDays', scale: 86_400_000, maximum: 30, defaultValue: 7 },
    { key: 'modelTokens', label: 'goalBudgetTokens', scale: 1, maximum: 2_000_000, defaultValue: 100_000 },
    { key: 'modelCalls', label: 'goalBudgetModelCalls', scale: 1, maximum: 1_000, defaultValue: 160 },
    { key: 'toolCalls', label: 'goalBudgetToolCalls', scale: 1, maximum: 2_000, defaultValue: 200 },
    { key: 'slices', label: 'goalBudgetSlices', scale: 1, maximum: 200, defaultValue: 20 },
    { key: 'stalledSlices', label: 'goalBudgetStalledSlices', scale: 1, maximum: 10, defaultValue: 3 },
] as const;

function validPolicy(value: GoalBudgetPolicy): boolean {
    return value.schemaVersion === 1 && Number.isSafeInteger(value.revision)
        && fields.every(({ key, scale, maximum }) => value.limits[key] === null
            || (Number.isSafeInteger(value.limits[key]) && value.limits[key]! >= scale
                && value.limits[key]! <= scale * maximum));
}

export async function fetchGoalBudgetPolicy(path = '/api/my/ai-assistant-session/goal/budget-policy'): Promise<GoalBudgetPolicy> {
    const response = await fetch(path, { credentials: 'include', cache: 'no-store' });
    const body = await response.json();
    if (!response.ok || !body?.success || !body.data || !validPolicy(body.data)) {
        throw new Error(body?.message ?? 'Goal budget policy unavailable');
    }
    return body.data as GoalBudgetPolicy;
}

export default function GoalBudgetPolicySettings() {
    const { t } = useTranslation();
    const [current, setCurrent] = useState<GoalBudgetPolicy | null>(null);
    const [draft, setDraft] = useState<GoalBudgetLimits | null>(null);
    const [input, setInput] = useState<Record<keyof GoalBudgetLimits, string> | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [saved, setSaved] = useState(false);
    const adopt = (policy: GoalBudgetPolicy) => {
        setCurrent(policy);
        setDraft(policy.limits);
        setInput(Object.fromEntries(fields.map(({ key, scale, defaultValue }) => [key,
            String(policy.limits[key] === null ? defaultValue : policy.limits[key]! / scale)])) as Record<keyof GoalBudgetLimits, string>);
    };
    const reload = async () => {
        setBusy(true); setError(''); setSaved(false);
        try { adopt(await fetchGoalBudgetPolicy('/api/admin/system/goal-budget-policy')); }
        catch (cause) { setCurrent(null); setError(cause instanceof Error ? cause.message : 'Load failed'); }
        finally { setBusy(false); }
    };
    useEffect(() => { void reload(); }, []);
    const next = draft && input ? Object.fromEntries(fields.map(({ key, scale }) => {
        const number = Number(input[key]);
        return [key, draft[key] === null ? null : number * scale];
    })) as GoalBudgetLimits : null;
    const valid = next !== null && fields.every(({ key, scale, maximum }) =>
        next[key] === null || (Number.isSafeInteger(next[key]) && next[key]! >= scale && next[key]! <= scale * maximum));
    const dirty = !!current && !!next && fields.some(({ key }) => current.limits[key] !== next[key]);
    const save = async () => {
        if (!current || !next || !valid || !dirty || busy) return;
        setBusy(true); setError(''); setSaved(false);
        try {
            const response = await fetch('/api/admin/system/goal-budget-policy', {
                method: 'PUT', credentials: 'include',
                headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
                body: JSON.stringify({ expectedRevision: current.revision, limits: next }),
            });
            const body = await response.json();
            if (!response.ok || !body?.success || !validPolicy(body.data)) throw new Error(body?.message ?? 'Save failed');
            adopt(body.data);
            setSaved(true);
        } catch (cause) {
            setCurrent(null);
            setError(cause instanceof Error ? cause.message : 'Save failed');
        } finally { setBusy(false); }
    };
    return <Card>
        <CardHeader><CardTitle>{t('pages.aiAssistant.goalBudgetPolicyTitle')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.goalBudgetPolicyDescription')}</p>
            {fields.map(({ key, label, scale, maximum }) => <div key={key} className="flex flex-wrap items-center gap-3">
                <label className="flex min-w-56 items-center gap-2">
                    <Checkbox checked={draft?.[key] !== null && !!draft} disabled={!draft || busy}
                        onCheckedChange={(checked) => setDraft(previous => previous && ({
                            ...previous, [key]: checked === true ? Number(input?.[key] ?? 1) * scale : null,
                        }))} />
                    <span>{t(`pages.aiAssistant.${label}`)}</span>
                </label>
                <Input className="w-32" type="number" min={1} max={maximum} step={1}
                    value={input?.[key] ?? ''} disabled={!draft || draft[key] === null || busy}
                    onChange={(event) => setInput(previous => previous && ({ ...previous, [key]: event.target.value }))} />
                {draft?.[key] === null && <span className="text-sm text-muted-foreground">{t('pages.aiAssistant.goalBudgetDisabled')}</span>}
            </div>)}
            {error && <p role="alert" className="text-destructive">{error}</p>}
            {draft && !valid && <p role="alert" className="text-destructive">{t('pages.aiAssistant.goalBudgetInvalid')}</p>}
            {saved && <p role="status">{t('pages.aiAssistant.goalBudgetSaved')}</p>}
            <div className="flex gap-2">
                <Button onClick={() => void save()} disabled={!current || busy || !valid || !dirty}>{t('pages.aiAssistant.goalBudgetSave')}</Button>
                <Button variant="outline" onClick={() => void reload()} disabled={busy}>{t('pages.aiAssistant.goalBudgetReload')}</Button>
            </div>
        </CardContent>
    </Card>;
}
