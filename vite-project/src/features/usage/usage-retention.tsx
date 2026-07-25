import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, Archive } from 'lucide-react';
import { useQueryClient } from '@tanstack/react-query';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { useToast } from '@/hooks/use-toast';
import {
    useGetUsageRetention,
    getUsageRetentionQueryKey,
} from '@/services/hooks/usageRetentionController/useGetUsageRetention';
import { useUpdateUsageRetention } from '@/services/hooks/usageRetentionController/useUpdateUsageRetention';
import {
    MIN_RETENTION_DAYS as MIN_DAYS,
    MAX_RETENTION_DAYS as MAX_DAYS,
    isValidRetentionDays,
} from '@/features/usage/usage-retention-validation';

/**
 * Retention config for the portable/signal server's local usage rollups. The
 * single-node server keeps last-writer-wins config (no revision), so this is a
 * plain read-then-save form: three windows in days, clamped client-side to a sane
 * range and re-validated by the backend.
 */
export function UsageRetentionPage() {
    const { t } = useTranslation();
    const { toast } = useToast();
    const queryClient = useQueryClient();

    const { data, isLoading } = useGetUsageRetention();
    const update = useUpdateUsageRetention();

    const [turnDays, setTurnDays] = useState('');
    const [aiDays, setAiDays] = useState('');
    const [agentSessionDays, setAgentSessionDays] = useState('');

    // Seed the form once the current config loads.
    useEffect(() => {
        const cfg = data?.data;
        if (cfg) {
            setTurnDays(String(cfg.turn_days));
            setAiDays(String(cfg.ai_days));
            setAgentSessionDays(String(cfg.agent_session_days));
        }
    }, [data]);

    const onSave = () => {
        const turn = Number(turnDays);
        const ai = Number(aiDays);
        const agentSession = Number(agentSessionDays);
        if (
            !isValidRetentionDays(turn) ||
            !isValidRetentionDays(ai) ||
            !isValidRetentionDays(agentSession)
        ) {
            toast({ variant: 'destructive', title: t('pages.usageRetention.invalidRange') });
            return;
        }
        update.mutate(
            {
                data: {
                    turn_days: turn,
                    ai_days: ai,
                    agent_session_days: agentSession,
                },
            },
            {
                onSuccess: () => {
                    toast({ title: t('pages.usageRetention.saved') });
                    void queryClient.invalidateQueries({ queryKey: getUsageRetentionQueryKey() });
                },
                onError: (err) => {
                    toast({
                        variant: 'destructive',
                        title: t('pages.usageRetention.saveFailed'),
                        description: err instanceof Error ? err.message : undefined,
                    });
                },
            },
        );
    };

    return (
        <div className="flex flex-col gap-6 p-6 max-w-4xl mx-auto w-full">
            <Card>
                <CardHeader>
                    <div className="flex items-center gap-2">
                        <Archive className="h-5 w-5 text-primary" />
                        <CardTitle>{t('pages.usageRetention.title')}</CardTitle>
                    </div>
                    <CardDescription>{t('pages.usageRetention.description')}</CardDescription>
                </CardHeader>
                <CardContent className="flex flex-col gap-4">
                    {isLoading ? (
                        <div className="text-muted-foreground text-sm py-8 text-center">
                            {t('pages.usageRetention.loading')}
                        </div>
                    ) : (
                        <>
                            <div className="flex flex-col gap-1 max-w-xs">
                                <Label htmlFor="turn-days">{t('pages.usageRetention.turnDays')}</Label>
                                <Input
                                    id="turn-days"
                                    type="number"
                                    min={MIN_DAYS}
                                    max={MAX_DAYS}
                                    value={turnDays}
                                    disabled={update.isPending}
                                    onChange={(e) => setTurnDays(e.target.value)}
                                />
                            </div>
                            <div className="flex flex-col gap-1 max-w-xs">
                                <Label htmlFor="ai-days">{t('pages.usageRetention.aiDays')}</Label>
                                <Input
                                    id="ai-days"
                                    type="number"
                                    min={MIN_DAYS}
                                    max={MAX_DAYS}
                                    value={aiDays}
                                    disabled={update.isPending}
                                    onChange={(e) => setAiDays(e.target.value)}
                                />
                            </div>
                            <div className="flex flex-col gap-1 max-w-xs">
                                <Label htmlFor="agent-session-days">
                                    {t('pages.usageRetention.agentSessionDays')}
                                </Label>
                                <Input
                                    id="agent-session-days"
                                    type="number"
                                    min={MIN_DAYS}
                                    max={MAX_DAYS}
                                    value={agentSessionDays}
                                    disabled={update.isPending}
                                    onChange={(e) => setAgentSessionDays(e.target.value)}
                                />
                            </div>
                            <div>
                                <Button onClick={onSave} disabled={update.isPending}>
                                    {update.isPending && <Loader2 className="h-4 w-4 mr-2 animate-spin" />}
                                    {t('pages.usageRetention.save')}
                                </Button>
                            </div>
                        </>
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
