import { Checkbox } from '@/components/ui/checkbox';
import { AlertTriangle } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { queryComputerUseCommunicationPolicy, updateComputerUseCommunicationPolicy } from '@/services/clients';
import type { ComputerUseCommunicationPolicy } from '@/services/types';

const groups = [
    { name: 'general', flags: ['enabled'] },
    { name: 'browser', flags: ['browser_semantic'] },
    { name: 'messages', flags: ['communication_handoff', 'communication_send'] },
] as const;

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
        <Card className="mb-6">
            <CardHeader>
                <CardTitle>{t('pages.communicationPolicy.title')}</CardTitle>
                <CardDescription>{t('pages.communicationPolicy.description')}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
                <Alert className="border-amber-500/50 bg-amber-500/10 text-amber-800 dark:border-amber-500/30 dark:text-amber-300 [&>svg]:text-amber-600 dark:[&>svg]:text-amber-400">
                    <AlertTriangle className="h-4 w-4" aria-hidden="true" />
                    <AlertDescription>{t('pages.communicationPolicy.localOnly')}</AlertDescription>
                </Alert>
                {local && <Button variant="outline" disabled={busy} onClick={() => void perform(false)}>{t('pages.communicationPolicy.load')}</Button>}
                {local && policy && <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); void perform(true); }}>
                    {groups.map((group) => <fieldset key={group.name} className="min-w-0 rounded-lg border p-4">
                        <legend className="px-1 text-sm font-medium">{t(`pages.communicationPolicy.group.${group.name}`)}</legend>
                        <div className="space-y-4">
                            {group.flags.map((flag) => <div key={flag}>
                                <label className="flex items-start gap-3 text-sm">
                                    <Checkbox  className="mt-1 shrink-0" checked={policy[flag]} disabled={busy} aria-describedby={`computer-use-${flag}-help`} onCheckedChange={nextChecked => {
                                        setPolicy({ ...policy, [flag]: (nextChecked === true) });
                                        setStatus(null);
                                    }} />
                                    <span>{t(`pages.communicationPolicy.${flag}`)}</span>
                                </label>
                                <p id={`computer-use-${flag}-help`} className="ml-6 mt-1 text-sm text-muted-foreground">{t(`pages.communicationPolicy.help.${flag}`)}</p>
                            </div>)}
                        </div>
                    </fieldset>)}
                    <p className="text-sm text-muted-foreground">{t(policy.enabled && policy.browser_semantic && policy.communication_send
                        ? 'pages.communicationPolicy.sendEnabled' : 'pages.communicationPolicy.sendDisabled')}</p>
                    <Button type="submit" disabled={busy}>{t('pages.communicationPolicy.save')}</Button>
                </form>}
                {status && <p role="status" className="text-sm">{t(`pages.communicationPolicy.${status}`)}</p>}
            </CardContent>
        </Card>
    );
}
