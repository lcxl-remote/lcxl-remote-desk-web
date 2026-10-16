import { useEffect, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Save, RefreshCw } from 'lucide-react';
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { useToast } from '@/hooks/use-toast';
import * as clients from '@/services/clients';
import { approvalAccessError, approvalRevisionConflict } from './api';
import { useOwnApprovalAccess } from './reviews';

type RuntimeClients = Pick<typeof clients, 'getApprovalRuntimeSettings' | 'updateApprovalRuntimeSettings'>;
export function createApprovalRuntimeApi(value: RuntimeClients) {
    function data<T>(result: { success?: boolean; data?: T | null; code?: number; message?: string | null }): T {
        if (!result.success || result.data == null) throw Object.assign(new Error(result.message || 'Approval settings unavailable'), { code: result.code });
        return result.data;
    }
    return {
        load: (signal?: AbortSignal) => value.getApprovalRuntimeSettings({ signal }).then(data),
        save: (settings: Parameters<RuntimeClients['updateApprovalRuntimeSettings']>[0]) => value.updateApprovalRuntimeSettings(settings).then(data),
    };
}
const runtimeApi = createApprovalRuntimeApi(clients);
export function parseFormatRetryCount(value: string): number | null {
    if (!/^(0|[1-9]\d*)$/.test(value)) return null;
    const number = Number(value);
    return Number.isInteger(number) && number <= 4294967295 ? number : null;
}
export function ApprovalRuntimeSettingsPanel({ api = runtimeApi }: { api?: ReturnType<typeof createApprovalRuntimeApi> }) {
    const { identity } = useOwnApprovalAccess();
    const { t } = useTranslation();
    const { toast } = useToast();
    const client = useQueryClient();
    const key = ['approval-runtime-settings', location.origin, identity];
    const [draft, setDraft] = useState<string | null>(null);
    const [busy, setBusy] = useState(false);
    const [blocked, setBlocked] = useState(false);
    const query = useQuery({ queryKey: key, queryFn: ({ signal }) => api.load(signal), retry: false, gcTime: 0, enabled: !blocked });
    useEffect(() => { setDraft(null); setBlocked(false); return () => { void client.cancelQueries({ queryKey: key }); client.removeQueries({ queryKey: key }); }; }, [client, identity]);
    const text = draft ?? String(query.data?.format_retry_count ?? '');
    const count = parseFormatRetryCount(text);
    async function save() {
        if (!query.data || count == null) return;
        setBusy(true);
        try {
            const saved = await api.save({ ...query.data, format_retry_count: count });
            client.setQueryData(key, saved); setDraft(null);
            toast({ title: t('approval.saved') });
        } catch (error) {
            if (approvalAccessError(error)) { setBlocked(true); setDraft(null); client.removeQueries({ queryKey: key }); }
            if (approvalRevisionConflict(error)) await query.refetch();
            toast({ title: t('approval.failed'), description: t(approvalRevisionConflict(error) ? 'approval.runtimeConflict' : 'approval.failed'), variant: 'destructive' });
        } finally { setBusy(false); }
    }
    if (blocked || approvalAccessError(query.error)) return null;
    return <Card>
        <CardHeader><CardTitle>{t('approval.runtimeTitle')}</CardTitle><CardDescription>{t('approval.runtimeHint')}</CardDescription></CardHeader>
        <CardContent className="space-y-4">
            {query.isLoading && <p>{t('common.loading')}</p>}
            {query.error && <div role="alert"><p>{t('approval.failed')}</p><Button type="button" variant="outline" onClick={() => void query.refetch()}><RefreshCw className="mr-2 size-4"/>{t('approval.refresh')}</Button></div>}
            {query.data && <>
                <Label className="block space-y-2"><span>{t('approval.formatRetryCount')}</span><Input type="number" min={0} max={4294967295} step={1} value={text} disabled={busy} aria-invalid={count == null} onChange={event => setDraft(event.target.value)}/></Label>
                {count == null && <p role="alert" className="text-sm text-destructive">{t('approval.retryCountInvalid')}</p>}
                <Button type="button" disabled={busy || draft == null || count == null || query.isError} onClick={() => void save()}><Save className="mr-2 size-4"/>{t('common.save')}</Button>
            </>}
        </CardContent>
    </Card>;
}
