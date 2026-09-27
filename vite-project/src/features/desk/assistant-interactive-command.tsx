import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { SquareTerminal, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import type { GrantRequestItemDto, PermissionRequestDto } from '@/services/types';
import { ExecPtyClient, type PtyCarrierPhase } from '@/features/exec/exec-pty-client';
import { ExecPtyTerminal } from '@/features/exec/exec-pty-terminal';

/** Where the owner's terminal carrier for a pending interactive command runs. */
export type InteractiveCommandContext = {
    /** Server conversation id the permission request belongs to. */
    runId: string | null;
    /** This client's signaling connection, or null before it is known. */
    browserConnectionId: string | null;
    deviceId?: string;
};

export function interactiveCommandItem(request: PermissionRequestDto): GrantRequestItemDto | null {
    const [item] = request.items;
    return request.items.length === 1 && item.commandConfirmation?.interactive ? item : null;
}

/**
 * Approval of an interactive (PTY) command: the owner first opens the terminal
 * carrier bound to exactly this pending permission, then approves with that
 * carrier. The terminal stays attached while the command runs; closing it
 * stops the command.
 */
export function AssistantInteractiveCommand({ request, item, context, canDecide, disabled, onApprove }: {
    request: PermissionRequestDto;
    item: GrantRequestItemDto;
    context: InteractiveCommandContext;
    canDecide: boolean;
    disabled: boolean;
    onApprove: (carrierId: string) => Promise<boolean>;
}) {
    const { t } = useTranslation();
    const interactive = item.commandConfirmation?.interactive;
    const [client, setClient] = useState<ExecPtyClient | null>(null);
    const [phase, setPhase] = useState<PtyCarrierPhase | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [pending, setPending] = useState(false);
    const current = useRef<ExecPtyClient | null>(null);

    useEffect(() => () => current.current?.dispose(), []);

    const ready = Boolean(interactive?.execRequestId && context.runId && context.browserConnectionId);

    const openAndApprove = async () => {
        if (!interactive || !context.runId || !context.browserConnectionId || pending) return;
        setPending(true);
        setError(null);
        current.current?.dispose();
        const next = new ExecPtyClient({
            onPhase: (value, message) => {
                setPhase(value);
                if (value === 'error' && message) setError(message);
            },
            onOpened: () => setPhase('opened'),
            onClosed: () => setPhase('closed'),
        });
        current.current = next;
        setClient(next);
        try {
            const carrierId = await next.prepare({
                browserConnectionId: context.browserConnectionId,
                targetConnectionId: interactive.targetConnectionId,
                execRequestId: interactive.execRequestId,
                deviceId: context.deviceId,
                permission: { runId: context.runId, requestId: request.requestId },
            });
            if (!await onApprove(carrierId)) {
                next.dispose();
                setClient(null);
                setPhase(null);
            }
        } catch (reason) {
            next.dispose();
            setClient(null);
            setError(reason instanceof Error ? reason.message : t('pages.aiAssistant.interactiveCommand.openFailed'));
        } finally {
            setPending(false);
        }
    };

    return (
        <div className="space-y-2" data-testid="assistant-interactive-command">
            <p className="flex items-center gap-1.5 text-xs font-medium">
                <SquareTerminal className="h-4 w-4" />
                {t('pages.aiAssistant.interactiveCommand.title')}
                {interactive?.elevated && (
                    <span className="rounded bg-amber-500/15 px-1.5 py-0.5 text-amber-700 dark:text-amber-300">
                        {t('pages.aiAssistant.interactiveCommand.elevated')}
                    </span>
                )}
            </p>
            <p className="text-xs text-muted-foreground">
                {t('pages.aiAssistant.interactiveCommand.description')}
            </p>
            {canDecide && request.state === 'pending' && !client && (
                <Button type="button" size="sm" className="gap-1.5 px-2.5" disabled={disabled || pending || !ready}
                    onClick={() => void openAndApprove()}>
                    <SquareTerminal className="h-4 w-4" />
                    {t('pages.aiAssistant.interactiveCommand.openAndApprove')}
                </Button>
            )}
            {!ready && canDecide && request.state === 'pending' && (
                <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.interactiveCommand.unavailable')}</p>
            )}
            {error && <p role="alert" className="text-xs text-destructive">{error}</p>}
            {client && (
                <div>
                    <ExecPtyTerminal client={client} />
                    {phase !== 'closed' && phase !== 'error' && (
                        <Button type="button" size="sm" variant="outline" className="mt-2 gap-1.5 px-2.5"
                            onClick={() => client.cancel()}>
                            <X className="h-4 w-4" />
                            {t('pages.aiAssistant.interactiveCommand.stop')}
                        </Button>
                    )}
                    {phase === 'closed' && (
                        <p className="mt-1 text-xs text-muted-foreground">{t('pages.aiAssistant.interactiveCommand.closed')}</p>
                    )}
                </div>
            )}
        </div>
    );
}
