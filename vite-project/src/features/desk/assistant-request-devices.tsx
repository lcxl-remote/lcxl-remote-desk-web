import { useTranslation } from 'react-i18next';
import { Check, Monitor } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import { isAiAssistantEnabled } from './ai-assistant-switch';

export type AssistantRequestDevice = {
    id: string;
    name: string;
    platform: string;
};

export function AssistantRequestDevices({ selected, onChange }: {
    selected: AssistantRequestDevice[];
    onChange: (devices: AssistantRequestDevice[]) => void;
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
    const choices = new Map([...selected.map(device => [device.id, device] as const), ...available]);
    return <DropdownMenu>
        <DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="sm" className="min-w-0">
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
                return <DropdownMenuItem key={device.id} role="menuitemcheckbox" aria-checked={checked}
                    onSelect={event => {
                        event.preventDefault();
                        onChange(checked ? selected.filter(value => value.id !== device.id) : [...selected, device]);
                    }}>
                    <Check className={checked ? 'text-primary' : 'invisible'} aria-hidden="true" />
                    {device.name}<span className="text-muted-foreground">{device.platform}</span>
                </DropdownMenuItem>;
            })}
        </DropdownMenuContent>
    </DropdownMenu>;
}
