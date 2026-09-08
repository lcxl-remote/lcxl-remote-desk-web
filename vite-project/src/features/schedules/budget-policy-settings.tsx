import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { ScheduleBudgetPolicy, TaskBudget } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';

const fields = ['max_runs_per_utc_day', 'max_calls_per_run', 'max_model_tokens_per_run', 'max_runtime_seconds'] as const;
const limits = [10000, 10000, Number.MAX_SAFE_INTEGER, 86400];
type Props = {
    load: () => Promise<ScheduleBudgetPolicy>;
    update: (revision: number, maximum: TaskBudget) => Promise<ScheduleBudgetPolicy>;
};

export function BudgetPolicySettings({ load, update }: Props) {
    const { t } = useTranslation();
    const [config, setConfig] = useState<ScheduleBudgetPolicy | null>(null);
    const [draft, setDraft] = useState<string[]>([]);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState(false);
    const [saved, setSaved] = useState(false);
    const generation = useRef(0);
    const pending = useRef(false);
    const adopt = (value: ScheduleBudgetPolicy) => {
        if (value.schema_version !== 1 || !Number.isSafeInteger(value.revision) || value.revision < 0
            || fields.some((key, index) => !Number.isSafeInteger(value.maximum[key])
                || value.maximum[key] < 1 || value.maximum[key] > limits[index])) throw new Error('Invalid budget policy');
        setConfig(value); setDraft(fields.map(key => String(value.maximum[key])));
    };
    const reload = async () => {
        if (pending.current) return;
        pending.current = true; setBusy(true); setError(false); setSaved(false);
        const current = ++generation.current;
        try { const value = await load(); if (current === generation.current) adopt(value); }
        catch { if (current === generation.current) { setConfig(null); setError(true); } }
        finally { if (current === generation.current) { pending.current = false; setBusy(false); } }
    };
    useEffect(() => {
        void reload();
        return () => { ++generation.current; pending.current = false; };
    }, [load]);
    const values = draft.map(Number);
    const valid = draft.length === fields.length && values.every((value, index) =>
        Number.isSafeInteger(value) && value >= 1 && value <= limits[index]);
    const dirty = config !== null && fields.some((key, index) => values[index] !== config.maximum[key]);
    const save = async () => {
        if (!config || !valid || !dirty || pending.current) return;
        pending.current = true; setBusy(true); setError(false); setSaved(false);
        const current = ++generation.current;
        const maximum = Object.fromEntries(fields.map((key, index) => [key, values[index]])) as TaskBudget;
        try {
            const value = await update(config.revision, maximum);
            if (current === generation.current) { adopt(value); setSaved(true); }
        } catch {
            // A lost response may follow a committed write. Require a fresh read
            // before another save; never retry a mutation automatically.
            if (current === generation.current) { setConfig(null); setError(true); }
        } finally { if (current === generation.current) { pending.current = false; setBusy(false); } }
    };
    return <Card>
        <CardHeader><CardTitle>{t('schedules.policy.title')}</CardTitle></CardHeader>
        <CardContent className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('schedules.policy.description')}</p>
            {fields.map((key, index) => <label className="block space-y-1" key={key}>
                <span>{t(`schedules.editor.${key}`)}</span>
                <Input type="number" min={1} max={limits[index]} step={1} value={draft[index] ?? ''}
                    disabled={!config || busy} onChange={event => {
                        setDraft(previous => previous.map((value, position) => position === index ? event.target.value : value));
                        setSaved(false);
                    }} />
            </label>)}
            <p className="text-sm">{t('schedules.policy.effect')}</p>
            {error && <p role="alert">{t('schedules.policy.error')}</p>}
            {config && !valid && <p role="alert">{t('schedules.policy.invalid')}</p>}
            {saved && <p role="status">{t('schedules.policy.saved')}</p>}
            <div className="flex gap-2">
                <Button onClick={() => void save()} disabled={!config || busy || !valid || !dirty}>{t('schedules.policy.save')}</Button>
                <Button variant="outline" onClick={() => void reload()} disabled={busy}>{t('schedules.policy.reload')}</Button>
            </div>
        </CardContent>
    </Card>;
}
