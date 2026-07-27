import { useTranslation } from 'react-i18next';
import { AlertTriangle, CheckCircle2, CircleSlash, Info, Loader2 } from 'lucide-react';

import { useGetTurnInfo } from '@/services/hooks/turnController/useGetTurnInfo';
import type { RejectedTurnInterface, TurnRuntimeInfo } from '@/services/types';
import { turnInterfaceFaultEnum, turnRuntimeStateEnum } from '@/services/types';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';

/**
 * What the host is doing about TURN right now, as opposed to what was saved.
 *
 * The two differ constantly — a relay is restarting, an address was rejected,
 * this startup mode never relays at all — and the settings form alone cannot
 * show any of it. The card is always rendered: "not relaying" is the answer
 * that most needs explaining, so hiding the card exactly then would leave the
 * page silent about the only thing the operator wants to know.
 */
export function TurnRuntimeStatus() {
    const { t } = useTranslation();
    const { data, isLoading, error } = useGetTurnInfo();
    const info = data?.data;

    return (
        <Card className="mb-8">
            <CardHeader>
                <div className="flex items-center justify-between gap-2">
                    <CardTitle>{t('pages.turn.runtime.title')}</CardTitle>
                    {info && <StateBadge state={info.state} />}
                </div>
                <CardDescription>{t('pages.turn.runtime.description')}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-4">
                {isLoading && (
                    <p className="flex items-center gap-2 text-sm text-muted-foreground">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        {t('pages.turn.runtime.loading')}
                    </p>
                )}
                {/* The endpoint is registered in every startup mode, so a failure
                    here is about reaching this server, not about TURN. */}
                {!isLoading && !info && (
                    <p className="text-sm text-muted-foreground">
                        {error ? t('pages.turn.runtime.unreachable') : t('pages.turn.runtime.loading')}
                    </p>
                )}
                {info && <RuntimeDetail info={info} />}
            </CardContent>
        </Card>
    );
}

function StateBadge({ state }: { state: TurnRuntimeInfo['state'] }) {
    const { t } = useTranslation();
    const variant = state === turnRuntimeStateEnum.running ? 'default' : 'secondary';
    return <Badge variant={state === turnRuntimeStateEnum.failed ? 'destructive' : variant}>{t(stateLabelKey(state))}</Badge>;
}

function RuntimeDetail({ info }: { info: TurnRuntimeInfo }) {
    const { t } = useTranslation();
    const running = info.state === turnRuntimeStateEnum.running;

    return (
        <>
            <p className="flex items-start gap-2 text-sm">
                {running ? (
                    <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-emerald-600" />
                ) : info.state === turnRuntimeStateEnum.failed ? (
                    <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-destructive" />
                ) : (
                    <CircleSlash className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
                )}
                <span className={running ? undefined : 'text-muted-foreground'}>
                    {t(stateDetailKey(info.state))}
                </span>
            </p>

            {/* Only a failure carries a cause, and it is the one actionable thing
                on the card, so it is shown verbatim rather than summarised. */}
            {info.last_error && (
                <p className="rounded-md bg-destructive/10 px-3 py-2 font-mono text-xs break-all text-destructive">
                    {info.last_error}
                </p>
            )}

            {running && (
                <dl className="grid gap-2 text-sm sm:grid-cols-2">
                    <div>
                        <dt className="text-muted-foreground">{t('pages.turn.runtime.servedInterfaces')}</dt>
                        <dd className="mt-1 space-y-1 font-mono text-xs">
                            {info.interfaces.map((iface) => (
                                <div key={`${iface.listen}-${iface.external}`}>
                                    {iface.listen} → {iface.external}
                                </div>
                            ))}
                        </dd>
                    </div>
                    {typeof info.uptime_secs === 'number' && (
                        <div>
                            <dt className="text-muted-foreground">{t('pages.turn.runtime.uptime')}</dt>
                            <dd className="mt-1">{formatUptime(info.uptime_secs, t)}</dd>
                        </div>
                    )}
                </dl>
            )}

            {info.rejected_interfaces.length > 0 && <RejectedInterfaces rejected={info.rejected_interfaces} />}
        </>
    );
}

/**
 * Configured entries the host refuses to serve.
 *
 * Reported whatever the state: a relay that is up while one address was
 * rejected looks entirely healthy otherwise, and an operator would go on
 * believing every address they configured is in use.
 */
function RejectedInterfaces({ rejected }: { rejected: RejectedTurnInterface[] }) {
    const { t } = useTranslation();
    return (
        <div className="space-y-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3">
            <p className="flex items-center gap-2 text-sm font-medium text-amber-700 dark:text-amber-500">
                <Info className="h-4 w-4 shrink-0" />
                {t('pages.turn.runtime.rejected')}
            </p>
            <ul className="space-y-2 text-sm">
                {rejected.map((entry) => (
                    <li key={entry.index} className="space-y-1">
                        <div className="font-mono text-xs">
                            #{entry.index + 1} {entry.interface.transport.toUpperCase()} {entry.interface.listen} →{' '}
                            {entry.interface.external}
                        </div>
                        <div className="text-muted-foreground">{t(faultLabelKey(entry.fault))}</div>
                        {/* The server's own wording names the field and a working
                            value; the localized line above says what kind of
                            problem it is. */}
                        <div className="font-mono text-xs text-muted-foreground break-all">{entry.detail}</div>
                    </li>
                ))}
            </ul>
        </div>
    );
}

function stateLabelKey(state: TurnRuntimeInfo['state']): string {
    switch (state) {
        case turnRuntimeStateEnum.running:
            return 'pages.turn.runtime.state.running';
        case turnRuntimeStateEnum.disabled:
            return 'pages.turn.runtime.state.disabled';
        case turnRuntimeStateEnum.unsupported:
            return 'pages.turn.runtime.state.unsupported';
        case turnRuntimeStateEnum['not-configured']:
            return 'pages.turn.runtime.state.notConfigured';
        case turnRuntimeStateEnum.failed:
            return 'pages.turn.runtime.state.failed';
    }
}

function stateDetailKey(state: TurnRuntimeInfo['state']): string {
    return `${stateLabelKey(state)}.detail`;
}

function faultLabelKey(fault: RejectedTurnInterface['fault']): string {
    switch (fault) {
        case turnInterfaceFaultEnum['transport-not-served']:
            return 'pages.turn.runtime.fault.transportNotServed';
        case turnInterfaceFaultEnum['listen-not-an-address']:
            return 'pages.turn.runtime.fault.listenNotAnAddress';
        case turnInterfaceFaultEnum['external-not-an-address']:
            return 'pages.turn.runtime.fault.externalNotAnAddress';
        case turnInterfaceFaultEnum['external-not-dialable']:
            return 'pages.turn.runtime.fault.externalNotDialable';
    }
}

/**
 * Coarse uptime — the card answers "has it been up", not "for how long exactly".
 *
 * The value is interpolated as `value` rather than `count` on purpose: `count`
 * would engage i18next's plural resolution, and these units are abbreviated
 * precisely so they need no plural form in either language.
 */
function formatUptime(seconds: number, t: (key: string, options?: Record<string, unknown>) => string): string {
    if (seconds < 60) return t('pages.turn.runtime.uptime.seconds', { value: seconds });
    if (seconds < 3600) return t('pages.turn.runtime.uptime.minutes', { value: Math.floor(seconds / 60) });
    if (seconds < 86400) return t('pages.turn.runtime.uptime.hours', { value: Math.floor(seconds / 3600) });
    return t('pages.turn.runtime.uptime.days', { value: Math.floor(seconds / 86400) });
}
