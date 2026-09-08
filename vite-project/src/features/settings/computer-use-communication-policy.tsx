import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { queryComputerUseCommunicationPolicy, updateComputerUseCommunicationPolicy } from '@/services/clients';
import type { ComputerUseCommunicationPolicy } from '@/services/types';

const flags = ['enabled', 'browser_semantic', 'communication_handoff', 'communication_send'] as const;

export function ComputerUseCommunicationPolicySettings() {
    const { t } = useTranslation();
    const [policy, setPolicy] = useState<ComputerUseCommunicationPolicy | null>(null);
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState<'saved' | 'failed' | null>(null);
    const pending = useRef(false);
    const generation = useRef(0);
    const local = ['localhost', '127.0.0.1', '[::1]'].includes(window.location.hostname);
    useEffect(() => {
        generation.current += 1;
        return () => { generation.current += 1; };
    }, []);

    const perform = async (save: boolean) => {
        if (!local || pending.current || (save && !policy)) return;
        pending.current = true;
        const current = generation.current;
        setBusy(true);
        setStatus(null);
        try {
            const response = save && policy
                ? await updateComputerUseCommunicationPolicy({
                    expected_revision: policy.revision,
                    enabled: policy.enabled,
                    browser_semantic: policy.browser_semantic,
                    communication_handoff: policy.communication_handoff,
                    communication_send: policy.communication_send,
                })
                : await queryComputerUseCommunicationPolicy();
            if (!response.data) throw new Error('Missing local communication policy');
            if (generation.current !== current) return;
            setPolicy(response.data);
            setStatus(save ? 'saved' : null);
        } catch {
            if (generation.current !== current) return;
            // Persistence may have succeeded before worker acknowledgement failed.
            // Require a new read; never replay an uncertain or stale edit.
            setPolicy(null);
            setStatus('failed');
        } finally {
            pending.current = false;
            if (generation.current === current) setBusy(false);
        }
    };

    return (
        <Card>
            <CardHeader>
                <CardTitle>{t('pages.communicationPolicy.title')}</CardTitle>
                <CardDescription>{t('pages.communicationPolicy.description')}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
                <p className="text-sm text-muted-foreground">{t('pages.communicationPolicy.localOnly')}</p>
                {local && <Button variant="outline" disabled={busy} onClick={() => void perform(false)}>{t('pages.communicationPolicy.load')}</Button>}
                {local && policy && <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); void perform(true); }}>
                    {flags.map((flag) => <label key={flag} className="flex items-center gap-2 text-sm">
                        <input type="checkbox" checked={policy[flag]} disabled={busy} onChange={(event) => {
                            setPolicy({ ...policy, [flag]: event.target.checked });
                            setStatus(null);
                        }} />
                        {t(`pages.communicationPolicy.${flag}`)}
                    </label>)}
                    <p className="text-sm text-muted-foreground">{t(policy.enabled && policy.browser_semantic && policy.communication_send
                        ? 'pages.communicationPolicy.sendEnabled' : 'pages.communicationPolicy.sendDisabled')}</p>
                    <Button type="submit" disabled={busy}>{t('pages.communicationPolicy.save')}</Button>
                </form>}
                {status && <p role="status" className="text-sm">{t(`pages.communicationPolicy.${status}`)}</p>}
            </CardContent>
        </Card>
    );
}
