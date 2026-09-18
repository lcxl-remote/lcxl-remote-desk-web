import { Disclosure } from '@/components/ui/disclosure';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RefreshCw } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { groupCapabilityInventory } from './ai-assistant-provider-inventory';
import { capabilityDescriptionKey } from './assistant-capability-copy';
import type { CapabilityInventoryEntry } from './use-ai-assistant-capabilities';

/** Inventory browsing never selects context or grants execution authority. */
export function AssistantCapabilityList({ entries, loading, error, refreshDisabled, onRefresh }: {
    entries: CapabilityInventoryEntry[];
    loading: boolean;
    error: boolean;
    refreshDisabled: boolean;
    onRefresh: () => void;
}) {
    const { t } = useTranslation();
    const [query, setQuery] = useState('');
    const groups = groupCapabilityInventory(entries);
    const search = query.trim().toLocaleLowerCase();
    const filtered = groups.map((group) => ({
        ...group,
        entries: group.entries.filter((entry) => [
            t(group.displayNameKey, { defaultValue: group.providerId }),
            t(entry.capability.display_name_key, { defaultValue: entry.capability.capability_id }),
            t(capabilityDescriptionKey(entry.capability.display_name_key), {
                defaultValue: t('pages.aiAssistant.workspace.descriptionUnavailable'),
            }),
            entry.provider_id, entry.capability.capability_id, entry.capability.tool_name,
            entry.reason ? t(`pages.aiAssistant.blockedReason.${entry.reason}`, { defaultValue: entry.reason }) : '',
        ].some((value) => value.toLocaleLowerCase().includes(search))),
    })).filter((group) => group.entries.length > 0);
    return (
        <section data-testid="ai-assistant-capability-inventory" className="space-y-4">
            <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.workspace.capabilityHint')}</p>
            <div className="flex items-center gap-2">
                <Input value={query} onChange={(event) => setQuery(event.target.value)}
                    aria-label={t('pages.aiAssistant.workspace.capabilitySearch')}
                    placeholder={t('pages.aiAssistant.workspace.capabilitySearch')} />
                <Button variant="outline" size="icon" onClick={onRefresh} disabled={refreshDisabled || loading}
                    aria-label={t('pages.aiAssistant.refreshCapabilities')}>
                    <RefreshCw className={`h-4 w-4 ${loading ? 'animate-spin' : ''}`} />
                </Button>
            </div>
            {error && <Alert variant="destructive"><AlertDescription>{t('pages.aiAssistant.capabilityLoadError')}</AlertDescription></Alert>}
            {loading && <Skeleton className="h-16 w-full" />}
            {!loading && !error && filtered.length === 0 && <p className="text-sm text-muted-foreground">
                {t(search ? 'pages.aiAssistant.workspace.noMatches' : 'pages.aiAssistant.capabilityEmpty')}
            </p>}
            {filtered.map((group) => (
                <section key={group.providerId} className="space-y-2" data-provider-state={group.state}>
                    <h3 className="flex items-center justify-between gap-2 text-sm font-medium">
                        {t(group.displayNameKey, { defaultValue: group.providerId })}
                        <span className="text-xs font-normal text-muted-foreground">{t('pages.aiAssistant.providerReadyCount', {
                            ready: group.readyCount, total: groups.find((item) => item.providerId === group.providerId)!.entries.length,
                        })}</span>
                    </h3>
                    {group.entries.map((entry) => (
                        <div key={entry.capability.capability_id} className="rounded-lg border p-3">
                            <div className="flex flex-wrap items-start justify-between gap-2">
                                <span className="min-w-0 text-sm font-medium [overflow-wrap:anywhere]">
                                    {t(entry.capability.display_name_key, { defaultValue: entry.capability.capability_id })}
                                </span>
                                <Badge variant={entry.ready ? 'secondary' : 'outline'}>
                                    {entry.ready ? t('pages.aiAssistant.providerState.ready')
                                        : t(`pages.aiAssistant.blockedReason.${entry.reason ?? 'unknown'}`, {
                                            defaultValue: entry.reason ?? t('pages.aiAssistant.providerState.unavailable'),
                                        })}
                                </Badge>
                            </div>
                            <code className="mt-2 block break-all text-xs text-muted-foreground">
                                {entry.capability.capability_id}
                            </code>
                            <p className="mt-2 text-sm text-muted-foreground">
                                {t(capabilityDescriptionKey(entry.capability.display_name_key), {
                                    defaultValue: t('pages.aiAssistant.workspace.descriptionUnavailable'),
                                })}
                            </p>
                            <Disclosure className="mt-2 text-xs text-muted-foreground" title={<>{t('pages.aiAssistant.workspace.technicalDetails')}</>} summaryClassName="cursor-pointer py-1">

                                <p className="mt-2 break-all">{entry.capability.tool_name}</p>
                                <p className="break-all">{entry.capability.display_name_key}</p>
                                <p>{entry.capability.effect} · {entry.capability.execution_locality}</p>
                                <p>{group.providerId} · v{group.version}</p>
                                <p>{t('pages.aiAssistant.providerBuiltIn')}: {t(`pages.aiAssistant.boolean.${String(entry.compiled)}`)} · {t('pages.aiAssistant.providerConnected')}: {t(`pages.aiAssistant.boolean.${String(entry.connected)}`)}</p>
                            </Disclosure>
                        </div>
                    ))}
                </section>
            ))}
        </section>
    );
}
