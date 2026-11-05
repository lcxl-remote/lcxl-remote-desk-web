import { useCallback, useEffect, useRef, useState, type ComponentType } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { Disclosure } from '@/components/ui/disclosure';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import type { IssueConnectionUpdate, IssueConnectionsPublic, IssueConnectionProbe } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';

const path = '/api/admin/system/issue-connections';
class InvalidSettings extends Error {}

export interface ConnectionAccountsProps {
    value: string[];
    onChange(value: string[]): void;
    disabled: boolean;
}

function draftsOf(config: IssueConnectionsPublic): IssueConnectionUpdate[] {
    return config.connections.map(({ has_token: _token, has_custom_ca: _ca, ...connection }) => ({
        ...connection, token: null, extra_ca_pem: null,
    }));
}

async function request<T>(endpoint: string, init?: RequestInit): Promise<T> {
    const response = await fetch(endpoint, { ...init, credentials: 'include', cache: 'no-store' });
    const body = await response.json();
    if (response.ok && body?.success === false && body.code === deskErrorCodeEnum.INVALID_PARAMS) {
        throw new InvalidSettings('Invalid external connection settings');
    }
    if (!response.ok || !body?.success || !body.data) throw new Error('External connection request failed');
    return body.data as T;
}

export function IssueConnectionsSettings({ Accounts }: { Accounts?: ComponentType<ConnectionAccountsProps> }) {
    const { t } = useTranslation();
    const [config, setConfig] = useState<IssueConnectionsPublic | null>(null);
    const [drafts, setDrafts] = useState<IssueConnectionUpdate[]>([]);
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState('');
    const [probes, setProbes] = useState<Record<string, IssueConnectionProbe>>({});
    const pending = useRef(false);
    const epoch = useRef(0);
    const adopt = (value: IssueConnectionsPublic) => {
        setConfig(value);
        setDrafts(draftsOf(value));
    };
    const load = useCallback(async () => {
        const version = ++epoch.current;
        pending.current = true; setBusy(true); setStatus(''); setProbes({});
        try {
            const value = await request<IssueConnectionsPublic>(path);
            if (epoch.current === version) adopt(value);
        } catch {
            if (epoch.current === version) { setConfig(null); setDrafts([]); setStatus('loadFailed'); }
        } finally {
            if (epoch.current === version) { pending.current = false; setBusy(false); }
        }
    }, []);
    useEffect(() => { void load(); return () => { ++epoch.current; }; }, [load]);
    const dirty = !!config && JSON.stringify(drafts) !== JSON.stringify(draftsOf(config));
    const edit = (update: (previous: IssueConnectionUpdate[]) => IssueConnectionUpdate[]) => {
        setStatus(''); setProbes({});
        setDrafts(update);
    };
    const change = (index: number, update: Partial<IssueConnectionUpdate>) =>
        edit(previous => previous.map((item, position) => position === index ? { ...item, ...update } : item));
    const save = async () => {
        if (!config || pending.current || !dirty) return;
        const version = epoch.current;
        pending.current = true; setBusy(true); setStatus(''); setProbes({});
        try {
            const value = await request<IssueConnectionsPublic>(path, {
                method: 'PUT', headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ expected_revision: config.revision, connections: drafts }),
            });
            if (epoch.current === version) { adopt(value); setStatus('saved'); }
        } catch (error) {
            if (epoch.current === version) {
                if (error instanceof InvalidSettings) setStatus('invalid');
                else { setConfig(null); setDrafts([]); setStatus('saveFailed'); }
            }
        } finally {
            if (epoch.current === version) { pending.current = false; setBusy(false); }
        }
    };
    const test = async (connection: string) => {
        if (pending.current || dirty) return;
        const version = epoch.current;
        pending.current = true; setBusy(true); setStatus('');
        try {
            const result = await request<IssueConnectionProbe>(`${path}/${encodeURIComponent(connection)}/test`, { method: 'POST' });
            if (epoch.current === version) setProbes(previous => ({ ...previous, [connection]: result }));
        } catch {
            if (epoch.current === version) setStatus('testFailed');
        } finally {
            if (epoch.current === version) { pending.current = false; setBusy(false); }
        }
    };
    return <div className="mx-auto max-w-3xl space-y-4 p-4">
        <div className="flex items-center justify-between gap-3">
            <h1 className="text-xl font-semibold">{t('pages.issueConnections.title')}</h1>
            <Button variant="outline" disabled={busy} onClick={() => void load()}>{t('pages.webSearch.reload')}</Button>
        </div>
        <p className="text-sm text-muted-foreground">{t('pages.issueConnections.description')}</p>
        {drafts.map((connection, index) => {
            const saved = config?.connections.find(item => item.connection_ref === connection.connection_ref);
            const probe = probes[connection.connection_ref];
            const id = `issue-connection-${index}`;
            return <Card key={connection.connection_ref}>
                <CardHeader className="flex-row items-center justify-between gap-3">
                    <CardTitle>{connection.name || t('pages.issueConnections.newConnection')}</CardTitle>
                    <Button variant="ghost" disabled={busy} onClick={() => edit(previous => previous.filter((_, position) => position !== index))}>{t('pages.issueConnections.remove')}</Button>
                </CardHeader>
                <CardContent className="space-y-4">
                    <div className="space-y-1">
                        <Label htmlFor={`${id}-platform`}>{t('pages.issueConnections.platform')}</Label>
                        <Select value={connection.platform} disabled={busy} onValueChange={platform => {
                            if (platform === 'gitea' || platform === 'github') change(index, { platform });
                        }}>
                            <SelectTrigger id={`${id}-platform`}><SelectValue /></SelectTrigger>
                            <SelectContent><SelectItem value="gitea">Gitea</SelectItem><SelectItem value="github">GitHub</SelectItem></SelectContent>
                        </Select>
                    </div>
                    <div className="flex items-center gap-3"><Switch id={`${id}-enabled`} checked={connection.enabled} disabled={busy} onCheckedChange={enabled => change(index, { enabled })} /><Label htmlFor={`${id}-enabled`}>{t('pages.issueConnections.enabled')}</Label></div>
                    {(['name', 'instance_url', 'mcp_url'] as const).map(field => <div key={field} className="space-y-1">
                        <Label htmlFor={`${id}-${field}`}>{t(`pages.issueConnections.${field}`)}</Label>
                        <Input id={`${id}-${field}`} value={connection[field]} disabled={busy} onChange={event => change(index, { [field]: event.target.value })} />
                    </div>)}
                    <div className="space-y-1">
                        <Label htmlFor={`${id}-token`}>{t('pages.issueConnections.token')}</Label>
                        <Input id={`${id}-token`} type="password" autoComplete="new-password"
                            disabled={busy} value={connection.token ?? ''}
                            placeholder={saved?.has_token ? t('pages.issueConnections.keepToken') : ''}
                            onChange={event => change(index, { token: event.target.value || null })} />
                        {saved?.has_token && <Button variant="ghost" disabled={busy}
                            onClick={() => change(index, { token: '' })}>
                            {t('pages.issueConnections.clearToken')}
                        </Button>}
                    </div>
                    {Accounts && <Accounts value={connection.allowed_actor_ids} disabled={busy} onChange={allowed_actor_ids => change(index, { allowed_actor_ids })} />}
                    <Disclosure title={t('pages.issueConnections.customCa')}>
                        <Textarea aria-label={t('pages.issueConnections.customCa')} disabled={busy} value={connection.extra_ca_pem ?? ''} placeholder={saved?.has_custom_ca ? t('pages.issueConnections.keepCa') : ''} onChange={event => change(index, { extra_ca_pem: event.target.value || null })} />
                        {saved?.has_custom_ca && <Button variant="ghost" disabled={busy} onClick={() => change(index, { extra_ca_pem: '' })}>{t('pages.issueConnections.clearCa')}</Button>}
                    </Disclosure>
                    <Button variant="outline" disabled={busy || dirty || !saved} onClick={() => void test(connection.connection_ref)}>{t('pages.issueConnections.test')}</Button>
                    {probe && <p role="status" className="text-sm">{t('pages.issueConnections.probe', { account: probe.account_login, read: t(probe.can_read_issues ? 'pages.issueConnections.available' : 'pages.issueConnections.unavailable'), reply: t(probe.can_reply ? 'pages.issueConnections.available' : 'pages.issueConnections.unavailable') })}</p>}
                </CardContent>
            </Card>;
        })}
        {status && <p role={status === 'saved' ? 'status' : 'alert'}>{t(`pages.issueConnections.${status}`)}</p>}
        {config && <div className="flex gap-2">
            <Button disabled={busy || !dirty} onClick={() => void save()}>{t('pages.webSearch.save')}</Button>
            <Button variant="outline" disabled={busy} onClick={() => edit(previous => [...previous, {
                platform: 'gitea', connection_ref: crypto.randomUUID(), name: '', enabled: false, instance_url: '', mcp_url: '', allowed_actor_ids: Accounts ? [] : ['1'], token: null, extra_ca_pem: null,
            }])}>{t('pages.issueConnections.add')}</Button>
        </div>}
    </div>;
}
