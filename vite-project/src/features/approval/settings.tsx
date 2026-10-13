import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Save, RefreshCw } from 'lucide-react';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { useToast } from '@/hooks/use-toast';
import { approvalApi, approvalAccessError, approvalRevisionConflict, type ApprovalApi } from './api';
import { useOwnApprovalAccess } from './reviews';
import type { ApprovalTraceSettings } from '@/services/types';

export function ApprovalTraceSettingsPanel({ api = approvalApi, identity: suppliedIdentity }: { api?: ApprovalApi; identity?: string }) {
    const access = useOwnApprovalAccess();
    const identity = suppliedIdentity ?? access.identity;
    const { t } = useTranslation();
    const { toast } = useToast();
    const client = useQueryClient();
    const key = ['approval-settings', location.origin, identity];
    const [blocked, setBlocked] = useState(false);
    const query = useQuery({ queryKey: key, queryFn: ({ signal }) => api.settings(signal), enabled: access.allowed && !blocked, retry: false, gcTime: 0 });
    const [draft, setDraft] = useState<ApprovalTraceSettings | null>(null);
    const [busy, setBusy] = useState(false);
    useEffect(() => { setDraft(null); setBlocked(false); return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }; }, [client, identity]);
    const value = draft ?? query.data;
    async function save() {
        if (!value) return;
        setBusy(true);
        try {
            const saved = await api.save(value);
            client.setQueryData(key, saved); setDraft(null);
            toast({ title: t('approval.saved') });
        } catch (error) {
            if (approvalAccessError(error)) { setBlocked(true); setDraft(null); void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }
            if (approvalRevisionConflict(error)) { await query.refetch(); setDraft(null); }
            toast({ title: t('approval.failed'), description: t(approvalRevisionConflict(error) ? 'approval.conflict' : 'approval.failed'), variant: 'destructive' });
        } finally { setBusy(false); }
    }
    if (!access.allowed || blocked || approvalAccessError(query.error)) return null;
    return <Card>
        <CardHeader><CardTitle>{t('approval.settings')}</CardTitle><CardDescription>{t('approval.settingsHint')}</CardDescription></CardHeader>
        <CardContent className="space-y-4">
            {query.isLoading && <p>{t('common.loading')}</p>}
            {query.error && <div role="alert"><p>{t('approval.failed')}</p><Button type="button" variant="outline" onClick={() => void query.refetch()}><RefreshCw className="mr-2 size-4"/>{t('approval.refresh')}</Button></div>}
            {value && <>
                <Label className="flex items-center gap-3"><Switch checked={value.enabled} onCheckedChange={enabled => setDraft({ ...value, enabled })} disabled={busy}/>{t('approval.recordEnabled')}</Label>
                <div className="grid gap-3 sm:grid-cols-3">
                    {[
                        { key: 'retention_days', label: 'approval.retention', number: value.retention_days, min: 1, max: 3650 },
                        { key: 'storage_budget_bytes', label: 'approval.storage', number: Number(value.storage_budget_bytes) / 1048576, min: 16, max: 16384 },
                        { key: 'max_trace_bytes', label: 'approval.singleSize', number: value.max_trace_bytes / 1048576, min: 0.25, max: 64 },
                    ].map(field => <Label key={field.key} className="space-y-2"><span>{t(field.label)}</span><Input type="number" min={field.min} max={field.max} step={field.key === 'max_trace_bytes' ? 0.25 : 1} disabled={busy} value={field.number} onChange={event => {
                        const number = Number(event.target.value);
                        setDraft({ ...value, [field.key]: field.key === 'storage_budget_bytes' ? String(number * 1048576) : field.key === 'max_trace_bytes' ? number * 1048576 : number });
                    }}/></Label>)}
                </div>
                <Button type="button" onClick={() => void save()} disabled={busy || !draft || query.isError}><Save className="mr-2 size-4"/>{t('common.save')}</Button>
            </>}
        </CardContent>
    </Card>;
}
