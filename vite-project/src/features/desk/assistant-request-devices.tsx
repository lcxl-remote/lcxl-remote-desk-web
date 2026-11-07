import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Check, Monitor } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { getAiAssistantDeviceRoster, updateAiAssistantDeviceRoster } from '@/services/clients';
import { isAiAssistantEnabled } from './ai-assistant-switch';

export type AssistantRequestDevice = {
    id: string;
    name: string;
    platform: string;
};

export function useConversationRequestDevices(
    scope: string | undefined,
    conversation: string | null,
    ensureConversation: () => string,
) {
    const client = useQueryClient();
    const connections = useListConnections({ query: { enabled: !!scope } });
    const key = (id: string | null) => ['ai-assistant-request-devices', scope, id] as const;
    const save = useMutation({
        mutationFn: async (input: { conversation: string; expectedRevision: number; targetRefs: string[] }) => {
            const response = await updateAiAssistantDeviceRoster(input);
            if (!response.success || !response.data) throw new Error('Device selection could not be saved');
            return response.data;
        },
        onSuccess: (data, input) => client.setQueryData(key(input.conversation), data),
    });
    const saving = save.isPending && save.variables?.conversation === conversation;
    const roster = useQuery({
        queryKey: key(conversation),
        enabled: !!scope && !!conversation && !saving,
        queryFn: async ({ signal }) => {
            const response = await getAiAssistantDeviceRoster({ conversation: conversation! }, { signal });
            if (!response.success || !response.data) throw new Error('Device selection unavailable');
            return response.data;
        },
    });
    const ready = !scope || (!saving && (!conversation || !!roster.data));
    return {
        ready,
        selected: roster.data?.targetRefs.map(id => {
            const peer = connections.data?.data?.find(item => item.device_id === id)?.peer;
            return { id, name: peer?.display_name || id, platform: peer?.operation_system ?? '' };
        }) ?? [],
        update(devices: AssistantRequestDevice[]) {
            if (!ready) return;
            save.mutate({
                conversation: ensureConversation(),
                expectedRevision: roster.data?.revision ?? 0,
                targetRefs: devices.map(device => device.id),
            });
        },
        error: (save.isError && save.variables?.conversation === conversation)
            ? 'pages.aiAssistant.workbench.saveFailed'
            : roster.isError ? 'pages.aiAssistant.conversations.loadFailed' : null,
        reload() {
            save.reset();
            void roster.refetch();
        },
    };
}

export function AssistantRequestDevices({ selected, onChange, disabled, showStatus }: {
    selected: AssistantRequestDevice[];
    onChange: (devices: AssistantRequestDevice[]) => void;
    disabled?: boolean;
    showStatus?: boolean;
}) {
    const { t } = useTranslation();
    const connections = useListConnections();
    const available = new Map<string, AssistantRequestDevice>();
    for (const connection of connections.data?.data ?? []) {
        if (connection.device_id && isAiAssistantEnabled(connection.ai_assistant_settings)) {
            available.set(connection.device_id, {
                id: connection.device_id,
                name: connection.peer.display_name || t('pages.deskList.unnamedConnection'),
                platform: connection.peer.operation_system,
            });
        }
    }
    const connected = new Set(connections.data?.data?.map(item => item.device_id));
    const choices = new Map([...selected.map(device => [device.id, device] as const), ...available]);
    return <DropdownMenu>
        <DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="sm" className="min-w-0" disabled={disabled}>
                <Monitor className="h-4 w-4 shrink-0" aria-hidden="true" />
                <span className="truncate">{t('pages.aiAssistant.requestDevices')}</span>
                {selected.length > 0 && <span>{selected.length}</span>}
            </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
            {choices.size === 0 && <DropdownMenuItem disabled>
                {t(connections.isError ? 'pages.aiAssistant.conversations.loadFailed' : 'pages.aiAssistant.requestDevicesEmpty')}
            </DropdownMenuItem>}
            {[...choices.values()].map(device => {
                const checked = selected.some(value => value.id === device.id);
                return <DropdownMenuItem key={device.id} role="menuitemcheckbox" aria-checked={checked} disabled={disabled}
                    onSelect={event => {
                        event.preventDefault();
                        onChange(checked ? selected.filter(value => value.id !== device.id) : [...selected, device]);
                    }}>
                    <Check className={checked ? 'text-primary' : 'invisible'} aria-hidden="true" />
                    {device.name}<span className="text-muted-foreground">{device.platform}</span>
                    {showStatus && connections.isSuccess && <span className="text-muted-foreground">
                        {t(connected.has(device.id) ? 'pages.aiAssistant.workbench.deviceOnline'
                            : 'pages.aiAssistant.workbench.deviceOffline')}
                    </span>}
                </DropdownMenuItem>;
            })}
        </DropdownMenuContent>
    </DropdownMenu>;
}
