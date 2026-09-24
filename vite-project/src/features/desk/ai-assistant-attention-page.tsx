import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { AlertCircle, ArrowRight, RefreshCw } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { assistantConnections } from '@/features/schedules/assistant-paths';

type AttentionReason =
    | 'goal_open_approval' | 'permission_approval' | 'goal_needs_input'
    | 'goal_budget' | 'goal_stalled' | 'goal_blocked' | 'goal_deadline_soon';

type AttentionItem = {
    attentionId: string;
    sessionId: string;
    clientConversationId: string | null;
    deviceId: string;
    reason: AttentionReason;
    updatedAtUnixMs: number;
    deadlineUnixMs: number | null;
};

type AttentionPage = { items: AttentionItem[]; hasMore: boolean };

export default function AiAssistantAttentionPage() {
    const { t } = useTranslation();
    const [items, setItems] = useState<AttentionItem[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(false);
    const [hasMore, setHasMore] = useState(false);
    const [loadingMore, setLoadingMore] = useState(false);
    const requestEpoch = useRef(0);
    const connections = useListConnections({ query: { refetchInterval: 10_000 } });
    const online = connections.isError ? [] : (connections.data ?? []);
    const connectionIds = useMemo(() => assistantConnections(online), [online]);

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
        <div className="mx-auto w-full max-w-4xl space-y-4 p-4 md:p-6">
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
            {items.map((item) => {
                const connection = connectionIds[item.deviceId];
                const search = item.clientConversationId
                    ? `?conversation=${encodeURIComponent(item.clientConversationId)}` : '';
                return (
                    <Card key={item.attentionId}>
                        <CardHeader className="pb-2">
                            <CardTitle className="flex items-center gap-2 text-base">
                                <AlertCircle className="h-4 w-4 shrink-0" />
                                {t(`pages.aiAssistantAttention.reasons.${item.reason}`)}
                            </CardTitle>
                        </CardHeader>
                        <CardContent className="flex flex-wrap items-center justify-between gap-3 text-sm">
                            <div className="space-y-1 text-muted-foreground">
                                <p>{t('pages.aiAssistantAttention.device')}: {item.deviceId}</p>
                                <p>{new Date(item.updatedAtUnixMs).toLocaleString()}</p>
                            </div>
                            {connection && item.clientConversationId ? (
                                <Button asChild size="sm">
                                    <Link to={`/desk/${encodeURIComponent(connection)}/assistant${search}`}>
                                        {t('pages.aiAssistantAttention.open')}<ArrowRight className="ml-2 h-4 w-4" />
                                    </Link>
                                </Button>
                            ) : (
                                <span className="text-muted-foreground">{t('pages.aiAssistantAttention.deviceOffline')}</span>
                            )}
                        </CardContent>
                    </Card>
                );
            })}
            {hasMore && <Button variant="outline" disabled={loadingMore}
                onClick={() => { setLoadingMore(true); void loadPage(items.length, true); }}>
                {t('pages.aiAssistantAttention.moreItems')}
            </Button>}
        </div>
    );
}
