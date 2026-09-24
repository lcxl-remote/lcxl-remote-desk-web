import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { ArrowRight, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { useQueryServerInfo } from '@/services/hooks/systemController/useQueryServerInfo';
import { clearSessionGrant } from './session-grant';
import { hasAiAssistantBrowserEntry } from './ai-assistant-features';
import { isAiAssistantEnabled } from './ai-assistant-switch';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';

export type AssistantDeviceChoice = {
    connectionId: string;
    name: string | null | undefined;
    description: string | null | undefined;
};

export function AiAssistantConversationChoices({
    devices,
    loading,
    error,
    refreshing,
    onRefresh,
}: {
    devices: AssistantDeviceChoice[];
    loading: boolean;
    error: boolean;
    refreshing: boolean;
    onRefresh: () => void;
}) {
    const { t } = useTranslation();

    return <AiAssistantHubLayout>
        <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="space-y-1">
                <h1 className="text-xl font-semibold">{t('pages.aiAssistant.overview.conversationsTitle')}</h1>
                <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.overview.conversationsDescription')}</p>
            </div>
            <Button variant="outline" size="sm" onClick={onRefresh} disabled={refreshing}>
                <RefreshCw className={`mr-2 h-4 w-4 ${refreshing ? 'animate-spin' : ''}`} aria-hidden="true" />
                {t('pages.aiAssistant.conversations.refresh')}
            </Button>
        </div>
        {loading ? <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
            {[0, 1, 2].map(index => <Skeleton key={index} className="h-32 rounded-xl" />)}
        </div> : error ? <p role="alert" className="rounded-lg border p-6 text-sm text-destructive">
            {t('pages.aiAssistant.conversations.loadFailed')}
        </p> : devices.length === 0 ? <p className="rounded-lg border p-6 text-sm text-muted-foreground">
            {t('pages.aiAssistant.conversations.empty')}
        </p> : <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
            {devices.map(device => <Card key={device.connectionId} className="flex flex-col justify-between gap-4 p-4">
                <CardContent className="space-y-1 p-0">
                    <h2 className="truncate font-medium" title={device.name || undefined}>
                        {device.name || t('pages.deskList.unnamedConnection')}
                    </h2>
                    {device.description && <p className="truncate text-xs text-muted-foreground" title={device.description}>
                        {device.description}
                    </p>}
                </CardContent>
                <Button asChild size="sm" className="w-full">
                    <Link to={`/desk/${encodeURIComponent(device.connectionId)}/assistant`}
                        onClick={() => clearSessionGrant(device.connectionId)}>
                        {t('pages.aiAssistant.conversations.open')}
                        <ArrowRight className="ml-2 h-4 w-4" aria-hidden="true" />
                    </Link>
                </Button>
            </Card>)}
        </div>}
    </AiAssistantHubLayout>;
}

export default function AiAssistantConversations() {
    const connections = useListConnections();
    const serverInfo = useQueryServerInfo();
    const enabled = hasAiAssistantBrowserEntry(serverInfo.data?.data?.ai_assistant);
    const devices = enabled ? (connections.data ?? [])
        .filter(connection => isAiAssistantEnabled(connection.version_info))
        .map(connection => ({
            connectionId: connection.connection_id,
            name: connection.version_info.display_name,
            description: connection.version_info.operation_system,
        })) : [];

    return <AiAssistantConversationChoices
        devices={devices}
        loading={connections.isLoading || serverInfo.isLoading}
        error={connections.isError || serverInfo.isError}
        refreshing={connections.isFetching || serverInfo.isFetching}
        onRefresh={() => { void connections.refetch(); void serverInfo.refetch(); }}
    />;
}
