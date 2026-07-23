import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
    ChevronDown,
    Download,
    Eye,
    Files,
    GripHorizontal,
    Keyboard,
    LockKeyhole,
    LogOut,
    ShieldOff,
    TerminalSquare,
    Upload,
} from 'lucide-react';
import { emit } from '@tauri-apps/api/event';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';

export type HostFileTransferSummary = {
    transfer_id: string;
    direction: 'upload' | 'download';
    file_name: string;
    transferred_bytes: number;
    total_bytes: number;
};

export type HostAccessSession = {
    connection_id: string;
    actor: {
        display_name: string | null;
        access_source: 'authenticated_account' | 'temporary_grant' | 'unknown';
    };
    started_at: string;
    desktop_view: boolean;
    remote_control: boolean;
    terminal_count: number;
    file_manager: boolean;
    transfers: HostFileTransferSummary[];
};

export type HostAccessSnapshot = {
    epoch: string;
    revision: number;
    indicator_enabled: boolean;
    total_session_count: number;
    sessions: HostAccessSession[];
    remote_access: {
        mode: 'unlocked' | 'locked' | 'recovery_locked';
        state_version: number;
        locked_at: string | null;
        durable: boolean;
        central_sync: 'not_required' | 'pending' | 'synced';
    };
};

type SnapshotWindow = Window & {
    __lcxlHostAccessSnapshot?: HostAccessSnapshot;
};

type ControlResult = {
    request_id: string;
    ok: boolean;
    error: string | null;
};

const CONTROL_EVENT = 'lcxl-host-access-control';
const CONTROL_RESULT_EVENT = 'lcxl-host-access-control-result';

export function activeKinds(session: HostAccessSession): string[] {
    const kinds: string[] = [];
    if (session.desktop_view) kinds.push('desktop');
    if (session.remote_control) kinds.push('control');
    if (session.terminal_count > 0) kinds.push('terminal');
    if (session.file_manager) kinds.push('files');
    if (session.transfers.length > 0) kinds.push('transfer');
    return kinds;
}

export function formatTransferBytes(value: number): string {
    if (value < 1024) return `${value} B`;
    if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
    if (value < 1024 * 1024 * 1024) return `${(value / 1024 / 1024).toFixed(1)} MiB`;
    return `${(value / 1024 / 1024 / 1024).toFixed(1)} GiB`;
}

export function makePageBackgroundTransparent(documentRef: Document): () => void {
    const elements = [
        documentRef.documentElement,
        documentRef.body,
        documentRef.getElementById('root'),
    ].filter((element): element is HTMLElement => element !== null);
    const previous = elements.map((element) => ({
        element,
        value: element.style.getPropertyValue('background'),
        priority: element.style.getPropertyPriority('background'),
    }));

    for (const element of elements) {
        element.style.setProperty('background', 'transparent', 'important');
    }

    return () => {
        for (const { element, value, priority } of previous) {
            if (value) {
                element.style.setProperty('background', value, priority);
            } else {
                element.style.removeProperty('background');
            }
        }
    };
}

export default function HostAccessStatusPage() {
    const { t } = useTranslation();
    const [snapshot, setSnapshot] = useState<HostAccessSnapshot | null>(
        () => (window as SnapshotWindow).__lcxlHostAccessSnapshot ?? null,
    );
    const [open, setOpen] = useState(false);
    const [pendingRequest, setPendingRequest] = useState<string | null>(null);
    const [controlError, setControlError] = useState<string | null>(null);
    const remoteAccessLocked = snapshot?.remote_access.mode !== 'unlocked';

    useEffect(() => makePageBackgroundTransparent(document), []);

    useEffect(() => {
        document.title = remoteAccessLocked
            ? 'lcxl-host-access:locked'
            : open
                ? 'lcxl-host-access:expanded'
                : 'lcxl-host-access:collapsed';
    }, [open, remoteAccessLocked]);

    useEffect(() => {
        const receive = (event: Event) => {
            setSnapshot((event as CustomEvent<HostAccessSnapshot>).detail);
        };
        window.addEventListener('lcxl-host-access-snapshot', receive);
        const current = (window as SnapshotWindow).__lcxlHostAccessSnapshot;
        if (current) setSnapshot(current);
        return () => window.removeEventListener('lcxl-host-access-snapshot', receive);
    }, []);

    useEffect(() => {
        const receive = (event: Event) => {
            const result = (event as CustomEvent<ControlResult>).detail;
            if (result.request_id !== pendingRequest) return;
            setPendingRequest(null);
            setControlError(result.ok ? null : result.error ?? t('hostAccess.controlFailed'));
        };
        window.addEventListener(CONTROL_RESULT_EVENT, receive);
        return () => window.removeEventListener(CONTROL_RESULT_EVENT, receive);
    }, [pendingRequest, t]);

    const requestControl = async (payload: Record<string, unknown>) => {
        const request_id = crypto.randomUUID();
        setPendingRequest(request_id);
        setControlError(null);
        try {
            await emit(CONTROL_EVENT, { request_id, ...payload });
        } catch (error) {
            setPendingRequest(null);
            setControlError(error instanceof Error ? error.message : String(error));
        }
    };

    const counts = useMemo(() => {
        const result = { desktop: 0, control: 0, terminal: 0, files: 0, transfer: 0 };
        for (const session of snapshot?.sessions ?? []) {
            for (const kind of activeKinds(session)) result[kind as keyof typeof result] += 1;
        }
        return result;
    }, [snapshot]);

    if (!snapshot || (snapshot.sessions.length === 0 && !remoteAccessLocked)) {
        return <div className="h-screen w-full bg-transparent" />;
    }

    if (remoteAccessLocked) {
        const recovery = snapshot.remote_access.mode === 'recovery_locked';
        const retryPersistence = !snapshot.remote_access.durable && !recovery;
        return (
            <main className="h-screen w-full overflow-hidden bg-transparent p-3 select-none">
                <Card className="relative overflow-hidden border-red-500/60 bg-card/95 shadow-xl">
                    <div
                        data-tauri-drag-region
                        className="absolute inset-x-0 top-0 z-10 flex h-7 cursor-grab items-center justify-center text-muted-foreground/70 active:cursor-grabbing"
                        title={t('hostAccess.drag')}
                        aria-label={t('hostAccess.drag')}
                    >
                        <GripHorizontal className="pointer-events-none h-4 w-4" aria-hidden="true" />
                    </div>
                    <CardHeader className="px-4 pb-4 pt-7">
                        <div className="flex items-center gap-3">
                            <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-red-500/15 text-red-600">
                                <LockKeyhole className="h-5 w-5" aria-hidden="true" />
                            </span>
                            <span className="min-w-0 flex-1">
                                <CardTitle className="text-base">{t('hostAccess.lockedTitle')}</CardTitle>
                                <span className="mt-1 block text-sm text-muted-foreground" aria-live="polite">
                                    {t(recovery ? 'hostAccess.recoveryLockedDescription' : 'hostAccess.lockedDescription')}
                                </span>
                            </span>
                        </div>
                        {snapshot.remote_access.central_sync === 'pending' && (
                            <Badge variant="secondary" className="mt-3 w-fit">
                                {t('hostAccess.centralSyncPending')}
                            </Badge>
                        )}
                        {retryPersistence && (
                            <p className="mt-3 rounded-md border border-red-500/40 bg-red-500/10 p-2 text-xs text-red-700" role="alert">
                                {t('hostAccess.notDurable')}
                            </p>
                        )}
                        <div className="mt-3 rounded-md bg-muted/60 p-3 text-xs text-muted-foreground">
                            <p className="font-medium text-foreground">{t('hostAccess.followUpTitle')}</p>
                            <ul className="mt-1 list-disc space-y-1 pl-4">
                                <li>{t('hostAccess.followUpAccount')}</li>
                                <li>{t('hostAccess.followUpToken')}</li>
                                <li>{t('hostAccess.followUpCode')}</li>
                            </ul>
                        </div>
                        {retryPersistence ? (
                            <Button
                                variant="destructive"
                                className="mt-3 w-full"
                                disabled={pendingRequest !== null}
                                onClick={() => void requestControl({ action: 'lock' })}
                            >
                                <LockKeyhole className="mr-2 h-4 w-4" aria-hidden="true" />
                                {pendingRequest ? t('hostAccess.applying') : t('hostAccess.retryLock')}
                            </Button>
                        ) : (
                            <Button
                                className="mt-3 w-full"
                                disabled={pendingRequest !== null}
                                onClick={() => void requestControl({
                                    action: 'unlock',
                                    expected_version: snapshot.remote_access.state_version,
                                })}
                            >
                                <ShieldOff className="mr-2 h-4 w-4" aria-hidden="true" />
                                {pendingRequest
                                    ? t('hostAccess.applying')
                                    : t('hostAccess.unlock')}
                            </Button>
                        )}
                        {controlError && (
                            <p className="mt-2 text-xs text-destructive" role="alert">{controlError}</p>
                        )}
                    </CardHeader>
                </Card>
            </main>
        );
    }

    return (
        <main className="h-screen w-full overflow-hidden bg-transparent p-3 select-none">
            <Card className="relative overflow-hidden border-amber-500/50 bg-card/95 shadow-xl">
                <div
                    data-tauri-drag-region
                    className="absolute inset-x-0 top-0 z-10 flex h-7 cursor-grab items-center justify-center text-muted-foreground/70 active:cursor-grabbing"
                    title={t('hostAccess.drag')}
                    aria-label={t('hostAccess.drag')}
                >
                    <GripHorizontal className="pointer-events-none h-4 w-4" aria-hidden="true" />
                </div>
                <Collapsible open={open} onOpenChange={setOpen}>
                    <CardHeader className="px-4 pb-4 pt-7">
                        <CollapsibleTrigger
                            className="flex w-full items-center gap-3 text-left"
                            aria-label={t('hostAccess.toggleDetails')}
                        >
                            <span className="relative flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-amber-500/15 text-amber-600">
                                <Eye className="h-5 w-5" aria-hidden="true" />
                                <span className="absolute right-0 top-0 h-2.5 w-2.5 rounded-full bg-emerald-500 ring-2 ring-background" />
                            </span>
                            <span className="min-w-0 flex-1">
                                <CardTitle className="text-base">{t('hostAccess.title')}</CardTitle>
                                <span className="mt-1 block text-sm text-muted-foreground" aria-live="polite">
                                    {t('hostAccess.sessionSummary', { count: snapshot.total_session_count })}
                                </span>
                            </span>
                            <ChevronDown
                                className={`h-5 w-5 text-muted-foreground transition-transform ${open ? 'rotate-180' : ''}`}
                                aria-hidden="true"
                            />
                        </CollapsibleTrigger>
                        <div className="mt-3 flex flex-wrap gap-1.5">
                            {counts.desktop > 0 && <ActivityBadge icon={Eye} label={t('hostAccess.desktop')} />}
                            {counts.control > 0 && <ActivityBadge icon={Keyboard} label={t('hostAccess.control')} />}
                            {counts.terminal > 0 && <ActivityBadge icon={TerminalSquare} label={t('hostAccess.terminal')} />}
                            {counts.files > 0 && <ActivityBadge icon={Files} label={t('hostAccess.files')} />}
                            {counts.transfer > 0 && <ActivityBadge icon={Upload} label={t('hostAccess.transfer')} />}
                        </div>
                        <Button
                            variant="destructive"
                            size="sm"
                            className="mt-3 w-full"
                            disabled={pendingRequest !== null}
                            onClick={() => void requestControl({ action: 'lock' })}
                        >
                            <LockKeyhole className="mr-2 h-4 w-4" aria-hidden="true" />
                            {pendingRequest ? t('hostAccess.applying') : t('hostAccess.lockAll')}
                        </Button>
                        {controlError && (
                            <p className="mt-2 text-xs text-destructive" role="alert">{controlError}</p>
                        )}
                    </CardHeader>
                    <CollapsibleContent>
                        <CardContent className="max-h-[285px] space-y-3 overflow-y-auto border-t p-4">
                            {snapshot.sessions.map((session) => (
                                <SessionDetails
                                    key={session.connection_id}
                                    session={session}
                                    disabled={pendingRequest !== null}
                                    onDisconnect={(connectionId) => void requestControl({
                                        action: 'disconnect',
                                        connection_id: connectionId,
                                    })}
                                />
                            ))}
                        </CardContent>
                    </CollapsibleContent>
                </Collapsible>
            </Card>
        </main>
    );
}

function ActivityBadge({
    icon: Icon,
    label,
}: {
    icon: typeof Eye;
    label: string;
}) {
    return (
        <Badge variant="secondary" className="gap-1">
            <Icon className="h-3.5 w-3.5" aria-hidden="true" />
            {label}
        </Badge>
    );
}

function SessionDetails({
    session,
    disabled,
    onDisconnect,
}: {
    session: HostAccessSession;
    disabled: boolean;
    onDisconnect: (connectionId: string) => void;
}) {
    const { t } = useTranslation();
    const actor = session.actor.display_name?.trim() || t('hostAccess.unknownActor');
    const source = t(`hostAccess.source.${session.actor.access_source}`);

    return (
        <section className="rounded-lg border bg-muted/30 p-3">
            <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                    <p className="truncate text-sm font-semibold">{actor}</p>
                    <p className="text-xs text-muted-foreground">{source}</p>
                </div>
                <code className="shrink-0 text-[10px] text-muted-foreground">
                    {session.connection_id.slice(-8)}
                </code>
            </div>
            <div className="mt-2 flex flex-wrap gap-1.5">
                {session.desktop_view && <ActivityBadge icon={Eye} label={t('hostAccess.desktop')} />}
                {session.remote_control && <ActivityBadge icon={Keyboard} label={t('hostAccess.control')} />}
                {session.terminal_count > 0 && (
                    <ActivityBadge icon={TerminalSquare} label={t('hostAccess.terminal')} />
                )}
                {session.file_manager && <ActivityBadge icon={Files} label={t('hostAccess.files')} />}
            </div>
            {session.transfers.length > 0 && (
                <div className="mt-3 space-y-2">
                    {session.transfers.map((transfer) => {
                        const DirectionIcon = transfer.direction === 'upload' ? Upload : Download;
                        return (
                            <div key={transfer.transfer_id} className="flex items-center gap-2 text-xs">
                                <DirectionIcon className="h-3.5 w-3.5 shrink-0 text-primary" aria-hidden="true" />
                                <span className="min-w-0 flex-1 truncate">{transfer.file_name}</span>
                                <span className="shrink-0 tabular-nums text-muted-foreground">
                                    {transfer.transferred_bytes > 0
                                        ? `${formatTransferBytes(transfer.transferred_bytes)} / ${formatTransferBytes(transfer.total_bytes)}`
                                        : formatTransferBytes(transfer.total_bytes)}
                                </span>
                            </div>
                        );
                    })}
                </div>
            )}
            <Button
                variant="outline"
                size="sm"
                className="mt-3 w-full"
                disabled={disabled}
                onClick={() => onDisconnect(session.connection_id)}
            >
                <LogOut className="mr-2 h-3.5 w-3.5" aria-hidden="true" />
                {t('hostAccess.disconnect')}
            </Button>
        </section>
    );
}
