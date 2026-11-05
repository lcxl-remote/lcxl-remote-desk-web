import { useEffect, useId, useRef, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useSearchParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { v4 } from 'uuid';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Textarea } from '@/components/ui/textarea';
import { createAiAssistantWorkbench, getAiAssistantDeviceRoster, listAiAssistantWorkbenches, updateAiAssistantDeviceRoster } from '@/services/clients';
import type { AiAssistantWorkbenchView } from '@/services/types';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';
import { AssistantRequestDevices, type AssistantRequestDevice } from './assistant-request-devices';
import { AssistantApprovalModePicker, subscriptionApprovalMode, useAssistantApprovalMode } from './assistant-approval-mode';
import { WorkbenchSubscriptions } from './assistant-workbench-subscriptions';
import { WorkbenchTasks } from './assistant-workbench-tasks';
import { useAiAssistantChat } from './use-ai-assistant-chat';
import { useDeskSignaling } from './use-desk-signaling';

const workbenchesKey = ['ai-assistant-workbenches'] as const;

export default function AssistantWorkbench() {
    const { t } = useTranslation();
    const [params, setParams] = useSearchParams();
    const nameId = useId();
    const [newName, setNewName] = useState('');
    const [creating, setCreating] = useState(false);
    const creation = useRef<{ name: string; id: string } | null>(null);
    const client = useQueryClient();
    const workbenches = useQuery({
        queryKey: workbenchesKey,
        queryFn: async ({ signal }) => {
            const result = await listAiAssistantWorkbenches({ signal });
            if (!result.success || !result.data) throw new Error('Workbench list unavailable');
            return result.data;
        },
    });
    const active = workbenches.data?.find(item => item.workbenchId === params.get('workbench'))
        ?? workbenches.data?.[0];
    const select = (id: string) => setParams(previous => {
        const next = new URLSearchParams(previous);
        next.set('workbench', id);
        next.delete('task');
        next.delete('worker');
        next.delete('permission');
        return next;
    });
    const create = useMutation({
        mutationFn: async () => {
            const name = newName.trim();
            if (creation.current?.name !== name) creation.current = { name, id: v4() };
            const result = await createAiAssistantWorkbench({ name, creationId: creation.current.id });
            if (!result.success || !result.data) throw new Error('Workbench creation failed');
            return result.data;
        },
        onSuccess: item => {
            client.setQueryData<AiAssistantWorkbenchView[]>(workbenchesKey, previous =>
                [...(previous ?? []).filter(row => row.workbenchId !== item.workbenchId), item]);
            void client.invalidateQueries({ queryKey: workbenchesKey });
            select(item.workbenchId);
            creation.current = null;
            setNewName('');
            setCreating(false);
        },
    });

    return <AiAssistantHubLayout>
        <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-2xl font-semibold">{t('pages.aiAssistant.workbench.title')}</h1>
            {active && <div className="flex flex-wrap items-center gap-2">
                <Select value={active.workbenchId} onValueChange={select}>
                    <SelectTrigger className="min-w-40" aria-label={t('pages.aiAssistant.workbench.name')}><SelectValue /></SelectTrigger>
                    <SelectContent>{workbenches.data?.map(item =>
                        <SelectItem key={item.workbenchId} value={item.workbenchId}>{item.name}</SelectItem>)}</SelectContent>
                </Select>
                <Button variant="outline" onClick={() => { create.reset(); setCreating(!creating); }}>
                    {t('pages.aiAssistant.workbench.create')}
                </Button>
            </div>}
        </div>
        {workbenches.isPending && <p role="status">{t('common.loading')}</p>}
        {workbenches.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.conversations.loadFailed')}</p>}
        {workbenches.isSuccess && (!active || creating) && <Card><CardContent className="pt-6">
            <form className="flex flex-wrap items-end gap-3" onSubmit={event => { event.preventDefault(); create.mutate(); }}>
                <div className="min-w-56 space-y-2">
                    <Label htmlFor={nameId}>{t('pages.aiAssistant.workbench.name')}</Label>
                    <Input id={nameId} required maxLength={80} value={newName} disabled={create.isPending}
                        onChange={event => setNewName(event.target.value)} />
                </div>
                <Button type="submit" disabled={create.isPending || !newName.trim()}>{t('pages.aiAssistant.workbench.create')}</Button>
                {create.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.workbench.saveFailed')}</p>}
            </form>
        </CardContent></Card>}
        {active && <WorkbenchTabs key={active.workbenchId} workbench={active} />}
    </AiAssistantHubLayout>;
}

function WorkbenchTabs({ workbench }: { workbench: AiAssistantWorkbenchView }) {
    const { t } = useTranslation();
    const [params, setParams] = useSearchParams();
    const selectedTab = params.get('tab');
    const tab = selectedTab === 'settings' || selectedTab === 'subscriptions' ? selectedTab : 'tasks';
    const approvalMode = useAssistantApprovalMode(workbench.workbenchId);
    const subscriptionMode = approvalMode.data ? subscriptionApprovalMode(approvalMode.data) : undefined;
    const request = params.get('task') ?? undefined;
    const changeView = (tab: string, task?: string) => setParams(previous => {
        const next = new URLSearchParams(previous);
        next.set('tab', tab);
        if (task) next.set('task', task);
        else next.delete('task');
        next.delete('worker');
        next.delete('permission');
        return next;
    });
    const rosterKey = ['ai-assistant-workbench-roster', workbench.conversationId] as const;
    const roster = useQuery({
        queryKey: rosterKey,
        queryFn: async ({ signal }) => {
            const result = await getAiAssistantDeviceRoster({ conversation: workbench.conversationId }, { signal });
            if (!result.success || !result.data) throw new Error('Device roster unavailable');
            return result.data;
        },
    });
    const client = useQueryClient();
    const [selection, setSelection] = useState<{ revision: number; devices: AssistantRequestDevice[] } | null>(null);
    const selected = selection?.devices ?? roster.data?.targetRefs.map(id => ({ id, name: id, platform: '' })) ?? [];
    const save = useMutation({
        mutationFn: async () => {
            if (!selection) throw new Error('Device roster unavailable');
            const result = await updateAiAssistantDeviceRoster({
                conversation: workbench.conversationId,
                expectedRevision: selection.revision,
                targetRefs: selection.devices.map(item => item.id),
            });
            if (!result.success || !result.data) throw new Error('Device roster save failed');
            return result.data;
        },
        onSuccess: data => {
            client.setQueryData(rosterKey, data);
            setSelection(null);
        },
    });
    return <Tabs value={tab} onValueChange={value => changeView(value)}>
        {approvalMode.data?.mode === 'no_review' && <Button type="button" size="sm" variant="destructive"
            className="mb-2 mr-2" onClick={() => changeView('settings')}>{t('pages.aiAssistant.approvalMode.no_review')}</Button>}
        <TabsList>
            <TabsTrigger value="tasks">{t('pages.aiAssistant.workbench.tasks')}</TabsTrigger>
            <TabsTrigger value="subscriptions">{t('pages.aiAssistant.workbench.subscriptions')}</TabsTrigger>
            <TabsTrigger value="settings">{t('pages.aiAssistant.workbench.settings')}</TabsTrigger>
        </TabsList>
        <TabsContent value="tasks" forceMount className="space-y-3 data-[state=inactive]:hidden">
            <WorkbenchComposer workbench={workbench} targetRefs={roster.data?.targetRefs} />
            <WorkbenchTasks key={request ?? 'list'} workbench={workbench.workbenchId} request={request}
                worker={params.get('worker') ?? undefined} permission={params.get('permission') ?? undefined} active={tab === 'tasks'} subscriptionMode={subscriptionMode}
                onOpen={id => changeView('tasks', id)} onClose={() => changeView('tasks')} />
        </TabsContent>
        <TabsContent value="subscriptions">
            <WorkbenchSubscriptions workbench={workbench.workbenchId} approvalMode={subscriptionMode} onOpenTask={id => changeView('tasks', id)} />
        </TabsContent>
        <TabsContent value="settings" className="space-y-3">
            <AssistantApprovalModePicker session={workbench.workbenchId} />
            {roster.isPending && <p role="status">{t('common.loading')}</p>}
            {roster.isError && <p role="alert" className="text-destructive">{t('pages.aiAssistant.conversations.loadFailed')}</p>}
            {roster.data && <div className="flex flex-wrap items-center gap-3">
                <AssistantRequestDevices selected={selected} disabled={save.isPending} showStatus
                    onChange={devices => setSelection(current => ({ revision: current?.revision ?? roster.data!.revision, devices }))} />
                <Button disabled={selection === null || save.isPending} onClick={() => save.mutate()}>
                    {t('pages.aiAssistant.workbench.saveDevices')}
                </Button>
            </div>}
            {save.isError && <div className="flex items-center gap-2">
                <p role="alert" className="text-destructive">{t('pages.aiAssistant.workbench.saveFailed')}</p>
                <Button variant="ghost" onClick={() => { setSelection(null); save.reset(); void roster.refetch(); }}>
                    {t('pages.aiAssistant.issueSources.reload')}
                </Button>
            </div>}
        </TabsContent>
    </Tabs>;
}

function WorkbenchComposer({ workbench, targetRefs }: { workbench: AiAssistantWorkbenchView; targetRefs?: string[] }) {
    const { t, i18n } = useTranslation();
    const id = useId();
    const { isConnected, subscribe, sendMessage } = useDeskSignaling();
    const chat = useAiAssistantChat({
        deskId: '', targetRefs, connected: isConnected, queueOnly: true,
        initialConversationId: workbench.conversationId,
        conversationStorageScope: `workbench:${workbench.workbenchId}`,
        subscribe, sendMessage,
    });
    const [text, setText] = useState('');
    const [lastSubmitted, setLastSubmitted] = useState<string | null>(null);
    useEffect(() => {
        if (chat.acceptedInput?.question !== lastSubmitted) return;
        setText(previous => previous.trim() === lastSubmitted ? '' : previous);
    }, [chat.acceptedInput, lastSubmitted]);

    return <Card><CardContent className="pt-6">
        <form className="space-y-3" onSubmit={event => {
            event.preventDefault();
            if (targetRefs && chat.start(text, i18n.language)) setLastSubmitted(text.trim());
        }}>
            <Label htmlFor={id}>{t('pages.aiAssistant.workbench.assign')}</Label>
            <Textarea id={id} required value={text} onChange={event => setText(event.target.value)} />
            <div className="flex items-center gap-3">
                <Button type="submit" disabled={!targetRefs || !isConnected || chat.hydrating || !!chat.deliveryState || !text.trim()}>
                    {t('pages.aiAssistant.workbench.assign')}
                </Button>
                {chat.deliveryState === 'unconfirmed' && <Button type="button" variant="outline" onClick={() => void chat.retryDelivery()}>
                    {t('pages.aiAssistant.workbench.retry')}
                </Button>}
                {lastSubmitted && chat.acceptedInput?.question === lastSubmitted && !chat.deliveryState &&
                    <p role="status">{t('pages.aiAssistant.workbench.accepted')}</p>}
            </div>
            {chat.error && <p role="alert" className="text-destructive">{chat.error}</p>}
        </form>
    </CardContent></Card>;
}
