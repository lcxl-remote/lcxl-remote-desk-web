import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { queryComputerUseApplicationPolicy, updateComputerUseApplicationPolicy } from '@/services/clients';
import type { ComputerUseApplicationPolicy } from '@/services/types';

export function ComputerUseApplicationPolicySettings() {
    const { t } = useTranslation();
    const [policy, setPolicy] = useState<ComputerUseApplicationPolicy | null>(null);
    const [restricted, setRestricted] = useState(false);
    const [paths, setPaths] = useState('');
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState<'saved' | 'failed' | null>(null);
    const local = ['localhost', '127.0.0.1', '[::1]'].includes(window.location.hostname);
    const apply = (next: ComputerUseApplicationPolicy) => {
        setPolicy(next);
        setRestricted(next.allowed_application_paths.length > 0);
        setPaths(next.allowed_application_paths.join('\n'));
    };
    const load = async () => {
        setBusy(true);
        setStatus(null);
        try {
            const response = await queryComputerUseApplicationPolicy();
            if (!response.data) throw new Error('Missing application policy');
            apply(response.data);
        } catch {
            setPolicy(null);
            setStatus('failed');
        } finally { setBusy(false); }
    };
    const save = async () => {
        if (!policy || busy) return;
        setBusy(true);
        setStatus(null);
        try {
            const response = await updateComputerUseApplicationPolicy({
                expected_revision: policy.revision,
                allowed_application_paths: restricted ? paths.split('\n').filter((path) => path.length > 0) : [],
            });
            if (!response.data) throw new Error('Missing application policy');
            apply(response.data);
            setStatus('saved');
        } catch {
            // Do not retry a stale edit or assume that persistence failure and
            // worker acknowledgement failure have the same outcome.
            setPolicy(null);
            setStatus('failed');
        } finally { setBusy(false); }
    };
    const emptyRestriction = restricted && !paths.split('\n').some((path) => path.length > 0);

    return (
        <Card>
            <CardHeader>
                <CardTitle>{t('pages.applicationPolicy.title')}</CardTitle>
                <CardDescription>{t('pages.applicationPolicy.description')}</CardDescription>
            </CardHeader>
            <CardContent>
                <details>
                    <summary className="cursor-pointer text-sm font-medium">{t('pages.applicationPolicy.advanced')}</summary>
                    <div className="mt-3 space-y-3">
                        <p className="text-sm text-muted-foreground">{t('pages.applicationPolicy.localOnly')}</p>
                        {local && <Button variant="outline" disabled={busy} onClick={() => void load()}>{t('pages.applicationPolicy.load')}</Button>}
                        {policy && (
                            <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); void save(); }}>
                                <label className="block text-sm">
                                    {t('pages.applicationPolicy.mode')}
                                    <select className="ml-2 rounded border bg-background p-2" value={restricted ? 'restricted' : 'unrestricted'} disabled={busy} onChange={(event) => setRestricted(event.target.value === 'restricted')}>
                                        <option value="unrestricted">{t('pages.applicationPolicy.unrestricted')}</option>
                                        <option value="restricted">{t('pages.applicationPolicy.restricted')}</option>
                                    </select>
                                </label>
                                {restricted && <label className="block text-sm">
                                    {t('pages.applicationPolicy.paths')}
                                    <textarea className="mt-1 block min-h-28 w-full rounded border bg-background p-2 font-mono" value={paths} disabled={busy} onChange={(event) => setPaths(event.target.value)} />
                                </label>}
                                <p className="text-xs text-muted-foreground">{t('pages.applicationPolicy.pathHelp')}</p>
                                <Button type="submit" disabled={busy || emptyRestriction}>{t('pages.applicationPolicy.save')}</Button>
                            </form>
                        )}
                        {status && <p role="status" className="text-sm">{t(`pages.applicationPolicy.${status}`)}</p>}
                    </div>
                </details>
            </CardContent>
        </Card>
    );
}
