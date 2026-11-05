import { useId, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import {
    getAiAssistantIssueSubscriptionAccounts, getMyIssueConnections, listAiAssistantIssueSubscriptions,
    saveAiAssistantIssueSubscription, setAiAssistantIssueSubscriptionEnabled,
} from '@/services/clients';
import { deskErrorCodeEnum, type IssueConnectionChoice, type IssueSubscriptionView, type IssueWorkKind } from '@/services/types';

export function WorkbenchSubscriptions({ workbench, approvalMode, onOpenTask }: {
    workbench: string;
    approvalMode?: 'manual' | 'ai_review';
    onOpenTask(request: string): void;
}) {
    const { t } = useTranslation();
    const client = useQueryClient();
    const queryKey = ['ai-assistant-workbench-subscriptions', workbench] as const;
    const [editing, setEditing] = useState<IssueSubscriptionView | 'new' | null>(null);
    const subscriptions = useQuery({
        queryKey,
        queryFn: async ({ signal }) => {
            const result = await listAiAssistantIssueSubscriptions({ workbench }, { signal });
            if (!result.success || !result.data) throw new Error('Subscription list unavailable');
            return result.data;
        },
        refetchInterval: 15_000,
    });
    const connections = useQuery({
        queryKey: ['ai-assistant-workbench-issue-connections'],
        queryFn: async ({ signal }) => {
            const result = await getMyIssueConnections({ signal });
            if (!result.success || !result.data) throw new Error('Issue connections unavailable');
            return result.data;
        },
    });
    const refresh = () => void client.invalidateQueries({ queryKey });
    const toggle = useMutation({
        mutationFn: async (item: IssueSubscriptionView) => {
            const result = await setAiAssistantIssueSubscriptionEnabled(item.subscriptionId, {
                expectedRevision: item.revision, enabled: !item.enabled,
            });
            if (!result.success || !result.data) throw new Error('Subscription update failed');
            return result.data;
        },
        onSettled: refresh,
    });

    return <div className="space-y-3">
        <Button disabled={!connections.data?.length || editing === 'new'} onClick={() => setEditing('new')}>
            {t('pages.aiAssistant.workbench.newSubscription')}
        </Button>
        {subscriptions.isPending && <p role="status">{t('common.loading')}</p>}
        {subscriptions.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.conversations.loadFailed')}</p>}
        {connections.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.issueSources.loadFailed')}</p>}
        {connections.data?.length === 0 && <p>{t('pages.aiAssistant.issueSources.empty')}</p>}
        {toggle.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.workbench.saveFailed')}</p>}
        {approvalMode && <p className="text-sm text-muted-foreground">
            {t('pages.aiAssistant.approvalMode.subscription', { mode: t(`pages.aiAssistant.approvalMode.${approvalMode}`) })}
        </p>}
        {editing && connections.data && <SubscriptionForm
            key={editing === 'new' ? 'new' : `${editing.subscriptionId}:${editing.revision}`}
            initial={editing === 'new' ? undefined : editing} workbench={workbench} connections={connections.data}
            onCancel={() => setEditing(null)} onSaved={() => { refresh(); setEditing(null); }}
            onConflict={item => { refresh(); setEditing(item); }} />}
        {subscriptions.data?.map(item => <Card key={item.subscriptionId}>
            <CardContent className="space-y-2 pt-6">
                <div className="flex flex-wrap items-center justify-between gap-2">
                    <h2 className="font-medium">{item.owner}/{item.repository}{item.issueNumber ? ` #${item.issueNumber}` : ''}</h2>
                    <div className="flex gap-2">
                        <Button size="sm" variant="ghost" onClick={() => setEditing(item)}>{t('pages.aiAssistant.workbench.edit')}</Button>
                        <Button size="sm" variant="outline" disabled={toggle.isPending} onClick={() => toggle.mutate(item)}>
                            {t(item.enabled ? 'pages.aiAssistant.workbench.pause' : 'pages.aiAssistant.workbench.resume')}
                        </Button>
                    </div>
                </div>
                <p className="text-sm text-muted-foreground">{connections.data?.find(choice => choice.connection_ref === item.connectionRef)?.name ?? item.connectionRef}
                    {' · '}{t(item.enabled ? 'pages.aiAssistant.workbench.enabled' : 'pages.aiAssistant.workbench.paused')}
                    {item.lastCheckedAtMs != null && ` · ${new Date(item.lastCheckedAtMs).toLocaleString()}`}
                </p>
                {!item.enabled && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.workbench.resumeBackfill')}</p>}
                {item.sourceError && <p role="status" className="text-sm text-destructive">{t('pages.aiAssistant.workbench.sourceFailed')}</p>}
                {item.latestIntake && <div className="flex flex-wrap items-center gap-2 text-sm">
                    <span>{item.latestIntake.state} · {item.latestIntake.reason}</span>
                    {item.latestIntake.requestId && <Button size="sm" variant="link" onClick={() => onOpenTask(item.latestIntake!.requestId!)}>
                        {t('pages.aiAssistant.workbench.openTask')}
                    </Button>}
                </div>}
            </CardContent>
        </Card>)}
    </div>;
}

function SubscriptionForm({ initial, workbench, connections, onCancel, onSaved, onConflict }: {
    initial?: IssueSubscriptionView;
    workbench: string;
    connections: IssueConnectionChoice[];
    onCancel(): void;
    onSaved(): void;
    onConflict(item: IssueSubscriptionView): void;
}) {
    const { t } = useTranslation();
    const id = useId();
    const [connection, setConnection] = useState(initial?.connectionRef ?? connections[0]?.connection_ref ?? '');
    const [repository, setRepository] = useState(initial ? `${initial.owner}/${initial.repository}` : '');
    const [issue, setIssue] = useState(initial?.issueNumber?.toString() ?? '');
    const [workKind, setWorkKind] = useState<IssueWorkKind>(initial?.workKind ?? 'analysis');
    const [replyAccount, setReplyAccount] = useState<number | null>(initial?.autoReply ? initial.outboundAccountId : null);
    const [login, setLogin] = useState('');
    const [senders, setSenders] = useState(() => [
        ...(initial?.allowedSenderIds ?? []).map(id => ({ id, login: `#${id}`, automation: false })),
        ...(initial?.automationSenderIds ?? []).map(id => ({ id, login: `#${id}`, automation: true })),
    ]);
    const [automation, setAutomation] = useState(false);
    const [overlap, setOverlap] = useState<IssueSubscriptionView | null>(null);
    const hasConnection = connections.some(item => item.connection_ref === connection);
    const accounts = useQuery({
        queryKey: ['ai-assistant-workbench-issue-accounts', workbench, connection],
        queryFn: async ({ signal }) => {
            const result = await getAiAssistantIssueSubscriptionAccounts({ workbench, connection }, { signal });
            if (!result.success || !result.data || !Number.isSafeInteger(result.data.outbound.id)) throw new Error('Account unavailable');
            return result.data;
        },
        enabled: hasConnection,
    });
    const addSender = useMutation({
        mutationFn: async () => {
            const result = await getAiAssistantIssueSubscriptionAccounts({ workbench, connection, login: login.trim() });
            const sender = result.data?.sender;
            if (!result.success || !sender || !Number.isSafeInteger(sender.id)) throw new Error('Sender unavailable');
            return { ...sender, automation: sender.automation || automation };
        },
        onSuccess: sender => {
            setSenders(previous => [...previous.filter(item => item.id !== sender.id), sender]);
            setLogin('');
            setAutomation(false);
        },
    });
    const [owner = '', repo = '', ...extra] = repository.trim().split('/');
    const issueNumber = issue.trim() ? Number(issue) : null;
    const validIssue = issueNumber === null || (Number.isSafeInteger(issueNumber) && issueNumber > 0 && /^[0-9]+$/.test(issue.trim()));
    const allowedSenderIds = senders.filter(sender => !sender.automation).map(sender => sender.id);
    const confirmedReply = replyAccount !== null && accounts.data?.outbound.id === replyAccount;
    const canSave = hasConnection && owner && repo && !extra.length && validIssue && allowedSenderIds.length > 0
        && (replyAccount === null || confirmedReply);
    const save = useMutation({
        mutationFn: async () => {
            setOverlap(null);
            const result = await saveAiAssistantIssueSubscription({
                subscriptionId: initial?.subscriptionId,
                expectedRevision: initial?.revision ?? 0,
                workbenchId: workbench,
                connectionRef: connection,
                owner, repository: repo, issueNumber, allowedSenderIds,
                automationSenderIds: senders.filter(sender => sender.automation).map(sender => sender.id),
                workKind, autoReply: confirmedReply, expectedOutboundAccountId: replyAccount,
                enabled: initial?.enabled ?? true,
            });
            if (result.success && result.data) { onSaved(); return; }
            if (result.code === deskErrorCodeEnum.REVISION_CONFLICT && result.data) { setOverlap(result.data); return; }
            throw new Error('Subscription save failed');
        },
    });
    const disabled = save.isPending || addSender.isPending;

    return <Card><CardContent className="pt-6">
        <form className="space-y-3" onSubmit={event => { event.preventDefault(); if (canSave && !disabled) save.mutate(); }}>
            <div className="space-y-1">
                <Label htmlFor={`${id}-connection`}>{t('pages.aiAssistant.issueSources.connection')}</Label>
                <Select value={connection} disabled={disabled} onValueChange={value => {
                    setConnection(value); setSenders([]); setReplyAccount(null); addSender.reset();
                }}>
                    <SelectTrigger id={`${id}-connection`}><SelectValue /></SelectTrigger>
                    <SelectContent>{connections.map(item => <SelectItem key={item.connection_ref} value={item.connection_ref}>{item.name}</SelectItem>)}</SelectContent>
                </Select>
            </div>
            <div className="grid gap-3 md:grid-cols-2">
                <div className="space-y-1">
                    <Label htmlFor={`${id}-repository`}>{t('pages.aiAssistant.workbench.repository')}</Label>
                    <Input id={`${id}-repository`} placeholder="owner/repository" required disabled={disabled}
                        value={repository} onChange={event => setRepository(event.target.value)} />
                </div>
                <div className="space-y-1">
                    <Label htmlFor={`${id}-issue`}>{t('pages.aiAssistant.workbench.issueNumber')}</Label>
                    <Input id={`${id}-issue`} inputMode="numeric" disabled={disabled}
                        value={issue} onChange={event => setIssue(event.target.value)} />
                </div>
            </div>
            <div className="space-y-2">
                <Label htmlFor={`${id}-login`}>{t('pages.aiAssistant.workbench.sender')}</Label>
                <div className="flex flex-wrap items-center gap-2">
                    <Input id={`${id}-login`} value={login} onChange={event => setLogin(event.target.value)} className="w-auto" disabled={disabled} />
                    <Button type="button" variant="outline" disabled={disabled || !hasConnection || !login.trim()} onClick={() => addSender.mutate()}>
                        {t('pages.aiAssistant.workbench.addSender')}
                    </Button>
                    <Checkbox id={`${id}-automation`} checked={automation} disabled={disabled} onCheckedChange={value => setAutomation(value === true)} />
                    <Label htmlFor={`${id}-automation`}>{t('pages.aiAssistant.workbench.automationAccount')}</Label>
                </div>
                {senders.length > 0 && <ul className="flex flex-wrap gap-2">{senders.map(sender => <li key={sender.id}>
                    <Button type="button" variant="secondary" size="sm" disabled={disabled}
                        onClick={() => setSenders(previous => previous.filter(item => item.id !== sender.id))}>
                        {sender.login}{sender.automation ? ` · ${t('pages.aiAssistant.workbench.automationAccount')}` : ''} ×
                    </Button>
                </li>)}</ul>}
                {addSender.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.workbench.accountFailed')}</p>}
            </div>
            <div className="space-y-1">
                <Label htmlFor={`${id}-kind`}>{t('pages.aiAssistant.workbench.workKind')}</Label>
                <Select value={workKind} disabled={disabled} onValueChange={value => setWorkKind(value === 'device_investigation' ? value : 'analysis')}>
                    <SelectTrigger id={`${id}-kind`}><SelectValue /></SelectTrigger>
                    <SelectContent>
                        <SelectItem value="analysis">{t('pages.aiAssistant.workbench.issueAnalysis')}</SelectItem>
                        <SelectItem value="device_investigation">{t('pages.aiAssistant.workbench.deviceInvestigation')}</SelectItem>
                    </SelectContent>
                </Select>
            </div>
            <div className="space-y-1">
                <div className="flex items-center gap-2">
                    <Checkbox id={`${id}-reply`} checked={replyAccount !== null} disabled={disabled || (replyAccount === null && !accounts.data)}
                        onCheckedChange={value => setReplyAccount(value === true ? accounts.data?.outbound.id ?? null : null)} />
                    <Label htmlFor={`${id}-reply`}>{t('pages.aiAssistant.workbench.autoReply')}</Label>
                </div>
                {accounts.data && <p className="text-sm text-muted-foreground">
                    {t('pages.aiAssistant.workbench.outboundAccount', { login: accounts.data.outbound.login })}
                </p>}
                {replyAccount !== null && !confirmedReply && <p role="status" className="text-sm text-destructive">
                    {t('pages.aiAssistant.workbench.accountFailed')}
                </p>}
            </div>
            <div className="flex flex-wrap items-center gap-2">
                <Button type="submit" disabled={!canSave || disabled}>{t(initial ? 'pages.aiAssistant.workbench.saveSubscription' : 'pages.aiAssistant.workbench.saveAndEnable')}</Button>
                <Button type="button" variant="ghost" disabled={disabled} onClick={onCancel}>{t('common.cancel')}</Button>
            </div>
            {save.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.workbench.saveFailed')}</p>}
            {overlap && <Button type="button" variant="link" onClick={() => onConflict(overlap)}>
                {t('pages.aiAssistant.workbench.openOverlap')}
            </Button>}
        </form>
    </CardContent></Card>;
}
