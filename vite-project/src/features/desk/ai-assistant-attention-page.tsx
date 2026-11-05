import { devicePath } from './device-target-paths';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { AlertCircle, ArrowRight, RefreshCw } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { assistantConnections } from '@/features/schedules/assistant-paths';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';

type AttentionReason =
    | 'goal_open_approval' | 'permission_approval' | 'goal_needs_input'
    | 'goal_budget' | 'goal_stalled' | 'goal_blocked' | 'goal_deadline_soon' | 'subagent_result' | 'worker_question' | 'issue_reply_needs_attention';

type AttentionItem = {
    attentionId: string;
    sessionId: string;
    clientConversationId: string | null;
    deviceId: string;
    reason: AttentionReason;
    requestId?: string | null;
    workbench?: { workbenchId: string; requestId: string; workerTaskId?: string | null } | null;
    updatedAtUnixMs: number;
    deadlineUnixMs: number | null;
};

type AttentionPage = { items: AttentionItem[]; hasMore: boolean };

export function assistantAttentionPath(item: AttentionItem, connected: boolean): string | undefined {
    if (item.workbench) {
        const { workbenchId, requestId, workerTaskId } = item.workbench;
        const params = new URLSearchParams({ workbench: workbenchId, task: requestId });
        if (workerTaskId) params.set('worker', workerTaskId);
        if (item.reason === 'permission_approval' && item.requestId) params.set('permission', item.requestId);
        return `/ai-assistant/workbench?${params}`;
    }
    if (item.clientConversationId && (connected || !item.deviceId)) {
        const path = item.deviceId ? devicePath(item.deviceId, 'assistant') : '/ai-assistant/conversation';
        return `${path}?conversation=${encodeURIComponent(item.clientConversationId)}`;
    }
}

export default function AiAssistantAttentionPage() {
    const { t } = useTranslation();
    const [items, setItems] = useState<AttentionItem[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(false);
    const [hasMore, setHasMore] = useState(false);
    const [loadingMore, setLoadingMore] = useState(false);
    const requestEpoch = useRef(0);
    const connections = useListConnections({ query: { refetchInterval: 10_000 } });
    const online = connections.isError ? [] : (connections.data?.data ?? []);
    const connectionIds = useMemo(() => assistantConnections(online), [online]);
    const groups = useMemo(() => {
        const groups = new Map<string, AttentionItem[]>();
        for (const item of items) {
            const key = item.workbench ? JSON.stringify([item.workbench.workbenchId, item.workbench.requestId]) : item.attentionId;
            const group = groups.get(key);
            if (group) group.push(item);
            else groups.set(key, [item]);
        }
        return [...groups];
    }, [items]);

    const loadPage = useCallback(async (offset: number, append: boolean) => {
        const epoch = ++requestEpoch.current;
        try {
            const response = await fetch(`/api/my/ai-assistant-attention?offset=${offset}&limit=100`, {
                credentials: 'include', headers: { Accept: 'application/json' },
            });
            const body = response.ok ? await response.json() : null;
            const page = body?.data as AttentionPage | undefined;
            if (!page || !Array.isArray(page.items) || typeof page.hasMore !== 'boolean') {
                throw new Error('Invalid attention list');
            }
            if (epoch !== requestEpoch.current) return;
            setItems(current => append
                ? [...current, ...page.items.filter(item => !current.some(earlier => earlier.attentionId === item.attentionId))]
                : page.items);
            setHasMore(page.hasMore);
            setError(false);
        } catch {
            if (epoch === requestEpoch.current) setError(true);
        } finally {
            if (append) setLoadingMore(false);
            if (epoch === requestEpoch.current) {
                setLoading(false);
            }
        }
    }, []);
    const refresh = useCallback(() => loadPage(0, false), [loadPage]);

    useEffect(() => {
        void refresh();
        const interval = window.setInterval(() => void refresh(), 15_000);
        const onFocus = () => void refresh();
        window.addEventListener('focus', onFocus);
        return () => {
            requestEpoch.current += 1;
            window.clearInterval(interval);
            window.removeEventListener('focus', onFocus);
        };
    }, [refresh]);

    return (
        <AiAssistantHubLayout>
            <div className="flex items-center justify-between gap-3">
                <div>
                    <h1 className="text-xl font-semibold">{t('pages.aiAssistantAttention.title')}</h1>
                    <p className="text-sm text-muted-foreground">{t('pages.aiAssistantAttention.description')}</p>
                </div>
                <Button variant="outline" size="sm" onClick={() => void refresh()}>
                    <RefreshCw className="mr-2 h-4 w-4" />{t('pages.aiAssistantAttention.refresh')}
                </Button>
            </div>
            {error && <p role="alert" className="text-sm text-destructive">{t('pages.aiAssistantAttention.loadFailed')}</p>}
            {loading && <p className="text-sm text-muted-foreground">{t('pages.aiAssistantAttention.loading')}</p>}
            {!loading && !error && items.length === 0 && (
                <p className="rounded-lg border p-6 text-sm text-muted-foreground">{t('pages.aiAssistantAttention.empty')}</p>
            )}
            {groups.map(([key, group]) => (
                    <Card key={key}>
                        <CardHeader className="pb-2">
                            <CardTitle className="flex items-center gap-2 text-base">
                                <AlertCircle className="h-4 w-4 shrink-0" />
                                {group[0].workbench ? t('pages.aiAssistant.workbench.attention')
                                    : t(`pages.aiAssistantAttention.reasons.${group[0].reason}`)}
                            </CardTitle>
                        </CardHeader>
                        <CardContent className="divide-y text-sm">
                            {group.map(item => {
                                const href = assistantAttentionPath(item, !!connectionIds[item.deviceId]);
                                return <div key={item.attentionId} className="flex flex-wrap items-center justify-between gap-3 py-2 first:pt-0 last:pb-0">
                                    <div className="space-y-1 text-muted-foreground">
                                        {item.workbench && <p>{t(`pages.aiAssistantAttention.reasons.${item.reason}`)}</p>}
                                        {item.deviceId && <p>{t('pages.aiAssistantAttention.device')}: {item.deviceId}</p>}
                                        <p>{new Date(item.updatedAtUnixMs).toLocaleString()}</p>
                                    </div>
                                    {href ? <Button asChild size="sm">
                                        <Link to={href}>{t('pages.aiAssistantAttention.open')}<ArrowRight className="ml-2 h-4 w-4" /></Link>
                                    </Button> : <span className="text-muted-foreground">
                                        {t(connections.isError ? 'pages.deskList.loadFailed' : connections.isPending ? 'common.loading' : 'pages.aiAssistantAttention.deviceOffline')}
                                    </span>}
                                </div>;
                            })}
                        </CardContent>
                    </Card>
            ))}
            {hasMore && <Button variant="outline" disabled={loadingMore}
                onClick={() => { setLoadingMore(true); void loadPage(items.length, true); }}>
                {t('pages.aiAssistantAttention.moreItems')}
            </Button>}
        </AiAssistantHubLayout>
    );
}
