import { appendBrowserPeerReport } from './browser-peer-report';
import { devicePath, sessionPath } from './device-target-paths';
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Outlet, useParams, useLocation, Link } from 'react-router-dom';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { isAxiosError } from 'axios';
import { useTranslation } from 'react-i18next';
import { getDeviceConnection } from '@/services/clients';
import type { ConnectionModel } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { errorCodeOf } from '@/lib/desk-error-i18n';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { readSessionGrant } from './session-grant';

export type DeviceConnectionResolution =
    | { status: 'loading'; connection: undefined }
    | { status: 'persistent'; connection: ConnectionModel; deviceKey: string }
    | { status: 'memory-only'; connection: ConnectionModel | undefined; deviceKey: null };

export interface DeviceTarget {
    kind: 'device' | 'session';
    orgId?: number;
    path: (page?: string) => string;
    connectionId: string;
    deviceId: string | undefined;
    connection: ConnectionModel | undefined;
    basePath: string;
    exitPath: string;
    claimSignalingSocket?: () => WebSocket | null;
    discoveryRevision?: number;
    resolution: DeviceConnectionResolution;
    /** New admissions wait for an in-flight refresh; established channels keep running. */
    assertCurrent: () => Promise<void>;
    refresh: () => Promise<void>;
}
const DeviceTargetContext = createContext<DeviceTarget | undefined>(undefined);
export const DeviceTargetProvider = DeviceTargetContext.Provider;
export function useOptionalDeviceTarget() { return useContext(DeviceTargetContext); }
export function useDeviceTarget() {
    const target = useOptionalDeviceTarget();
    if (!target) throw new Error('Device target boundary is required');
    return target;
}

function TargetUnavailable({ retry, finite = false, code, busy = false, redeemPath = '/user/login?tab=device_code' }: { retry?: () => void; finite?: boolean; code?: number; busy?: boolean; redeemPath?: string }) {
    const { t } = useTranslation();
    const key = code === deskErrorCodeEnum.REMOTE_DESK_OFFLINE ? 'pages.deviceTarget.offline'
        : code === deskErrorCodeEnum.SHARED_STORE_UNAVAILABLE ? 'pages.deviceTarget.storeUnavailable'
        : 'pages.deviceTarget.unavailable';
    return <div className="flex h-full flex-col items-center justify-center gap-4 p-6">
        <p role="alert">{t(finite ? 'pages.deviceTarget.sessionUnavailable' : key)}</p>
        {retry && <Button variant="outline" disabled={busy} onClick={retry}>{t('common.refresh')}</Button>}
        {finite && <Button asChild variant="outline"><Link to={redeemPath}>{t('pages.deviceTarget.redeemAgain')}</Link></Button>}
    </div>;
}

export function DeviceTargetBoundary({ accountKey, orgId, children }: { accountKey: string | null; orgId?: number; children?: ReactNode }) {
    const { deviceId = '' } = useParams<{ deviceId: string }>();
    const scope = JSON.stringify([accountKey, orgId ?? null, deviceId]);
    return <ResolvedDeviceTarget key={scope} accountKey={accountKey} orgId={orgId} deviceId={deviceId}>{children}</ResolvedDeviceTarget>;
}

function ResolvedDeviceTarget({ accountKey, orgId, deviceId, children }: { accountKey: string | null; orgId?: number; deviceId: string; children?: ReactNode }) {
    const location = useLocation();
    const socketRef = useRef<WebSocket | null>(null);
    const page = location.pathname.split('/').pop();
    // Authentication can overlap discovery. No host operation is sent until the
    // accepted target's admission check; the first transport takes ownership.
    useEffect(() => {
        if (accountKey == null || !['control', 'files', 'assistant'].includes(page ?? '')) return;
        const url = new URL('/api/desk/signaling', window.location.href);
        url.protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        appendBrowserPeerReport(url);
        const socket = new WebSocket(url.toString());
        socketRef.current = socket;
        return () => {
            if (socketRef.current === socket) { socketRef.current = null; socket.close(); }
        };
    }, [accountKey, deviceId, orgId, page]);
    const claimSignalingSocket = useCallback(() => {
        const socket = socketRef.current;
        socketRef.current = null;
        return socket && (socket.readyState === WebSocket.CONNECTING || socket.readyState === WebSocket.OPEN) ? socket : null;
    }, []);
    const client = useQueryClient();
    const queryKey = useMemo(() => ['device-target', accountKey, orgId ?? null, deviceId] as const, [accountKey, orgId, deviceId]);
    const query = useQuery({
        queryKey,
        queryFn: ({ signal }) => getDeviceConnection(deviceId, orgId == null ? { scope: 'personal' } : { scope: 'org', org_id: String(orgId) }, { signal }),
        enabled: accountKey != null && deviceId.length > 0,
        retry: false,
        refetchOnMount: 'always',
        staleTime: 0,
    });
    const [accepted, setAccepted] = useState<ConnectionModel | null>(null);
    const fresh = query.isSuccess && query.isFetchedAfterMount && !query.isFetching ? query.data?.data : undefined;
    const code = errorCodeOf(query.error);
    const status = isAxiosError(query.error) ? query.error.response?.status : undefined;
    const denied = code === deskErrorCodeEnum.DEVICE_NOT_FOUND || code === deskErrorCodeEnum.REMOTE_DESK_OFFLINE
        || code === deskErrorCodeEnum.PERMISSION_ERROR || status === 401 || status === 403;
    // Render-phase adjustment prevents committing one frame of the previous target.
    if (fresh && fresh !== accepted) setAccepted(fresh);
    if (denied && accepted) setAccepted(null);
    const active = denied ? null : fresh ?? accepted;
    useEffect(() => {
        if (query.isError && !accepted && socketRef.current) {
            socketRef.current.close(); socketRef.current = null;
        }
    }, [query.isError, accepted]);
    const alive = useRef(true);
    useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);
    const refresh = useCallback(async () => { await client.invalidateQueries({ queryKey, exact: true }, { cancelRefetch: false }); }, [client, queryKey]);
    const expected = active?.connection_id;
    const assertCurrent = useCallback(async () => {
        const current = client.getQueryCache().find({ queryKey, exact: true });
        if (current?.state.fetchStatus === 'fetching') await current.promise;
        const state = current?.state;
        const data = state?.data as typeof query.data;
        if (!alive.current || state?.status !== 'success' || state.fetchStatus !== 'idle'
            || !data?.success || !data.data || data.data.connection_id !== expected) {
            throw new Error('Device target must be resolved again');
        }
    }, [client, expected, queryKey]);
    const value = useMemo<DeviceTarget | undefined>(() => active ? {
        kind: 'device', orgId, claimSignalingSocket, discoveryRevision: query.dataUpdatedAt, path: (page = '') => devicePath(deviceId, page, orgId), connectionId: active.connection_id, deviceId,
        connection: active, basePath: devicePath(deviceId, '', orgId), exitPath: devicePath(deviceId, '', orgId),
        resolution: { status: 'persistent', connection: active, deviceKey: `device:${deviceId}` },
        assertCurrent, refresh,
    } : undefined, [active, assertCurrent, claimSignalingSocket, deviceId, orgId, query.dataUpdatedAt, refresh]);
    if (!value) return query.isPending || (query.isFetching && !denied)
        ? <Skeleton className="m-6 h-48" />
        : <TargetUnavailable code={code} busy={query.isFetching} retry={() => void refresh()} />;
    return <DeviceTargetContext.Provider value={value}>
        <TargetRuntime key={active!.connection_id}>{children ?? <Outlet />}</TargetRuntime>
    </DeviceTargetContext.Provider>;
}
function TargetRuntime({ children }: { children: ReactNode }) { return <div className="relative flex h-full min-h-0 flex-1 flex-col overflow-hidden">{children}</div>; }

/** Fixed grants never use owner discovery or the owner shell. */
export function FiniteTargetBoundary({ accountKey, children, redeemPath = '/user/login?tab=device_code' }: { accountKey: string | null; children: ReactNode; redeemPath?: string }) {
    const { connectionId = '' } = useParams<{ connectionId: string }>();
    const grant = readSessionGrant(connectionId);
    const alive = useRef(true);
    useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, [accountKey, connectionId]);
    const assertCurrent = useCallback(async () => {
        const current = readSessionGrant(connectionId);
        if (!alive.current || !current || current.principalKey !== accountKey) throw new Error('Session grant is unavailable');
    }, [accountKey, connectionId]);
    const refresh = useCallback(async () => {}, []);
    if (!grant || grant.principalKey !== accountKey) return <TargetUnavailable finite redeemPath={redeemPath} />;
    return <DeviceTargetContext.Provider value={{ kind: 'session', path: () => sessionPath(connectionId), connectionId, deviceId: undefined,
        connection: undefined, basePath: sessionPath(connectionId), exitPath: redeemPath,
        resolution: { status: 'memory-only', connection: undefined, deviceKey: null }, assertCurrent, refresh }}>
        <TargetRuntime key={`${accountKey}:${connectionId}`}>{children}</TargetRuntime>
    </DeviceTargetContext.Provider>;
}
