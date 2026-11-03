import { devicePath, sessionPath } from './device-target-paths';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { useEffect, useState } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { DeviceTargetBoundary, FiniteTargetBoundary, useDeviceTarget, type DeviceTarget } from './device-target-context';
import { saveSessionGrant, clearAllGrants } from './session-grant';
import type { ConnectionModel } from '@/services/types';

const get = vi.hoisted(() => vi.fn());
vi.mock('@/services/clients', () => ({ getDeviceConnection: get }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
function deferred<T>() { let resolve!: (v: T) => void; let reject!: (e: unknown) => void;
    return { promise: new Promise<T>((a,b) => { resolve=a; reject=b; }), resolve: (v:T) => resolve(v), reject: (e:unknown) => reject(e) }; }
const connection = (id: string) => ({ success: true, code: 0, data: { connection_id: id, device_id: 'd1', peer: { remote_desk_type: 'server' } } as ConnectionModel });
let current: DeviceTarget;
let mounts = 0;
let unmounts = 0;
function Runtime() {
    current = useDeviceTarget();
    const [selection, setSelection] = useState('empty');
    useEffect(() => { mounts++; return () => { unmounts++; }; }, []);
    return <button onClick={() => setSelection('old-command')}>{current.connectionId}:{selection}</button>;
}
function renderTarget(client: QueryClient, account = 'u:1', orgId?: number) {
    return render(<QueryClientProvider client={client}><MemoryRouter initialEntries={['/desk/device/d1']}>
        <Routes><Route path="/desk/device/:deviceId" element={<DeviceTargetBoundary accountKey={account} orgId={orgId}><Runtime /></DeviceTargetBoundary>} /></Routes>
    </MemoryRouter></QueryClientProvider>);
}
afterEach(() => { cleanup(); get.mockReset(); clearAllGrants(); mounts=0; unmounts=0; });

describe('stable device target admission and runtime', () => {
    it('waits for this mount even when a successful old connection is cached', async () => {
        const pending = deferred<ReturnType<typeof connection>>(); get.mockReturnValue(pending.promise);
        const client = new QueryClient(); client.setQueryData(['device-target','u:1',null,'d1'], connection('old'));
        renderTarget(client);
        expect(mounts).toBe(0);
        await act(async () => pending.resolve(connection('c2')));
        expect(await screen.findByText('c2:empty')).toBeInTheDocument();
        expect(get).toHaveBeenCalledExactlyOnceWith('d1', { scope: 'personal' }, expect.objectContaining({ signal: expect.any(AbortSignal) }));
    });

    it('keeps established runtime during same-connection refresh and transient failure, but blocks a new admission', async () => {
        get.mockResolvedValueOnce(connection('c1')); const client = new QueryClient(); renderTarget(client);
        await screen.findByText('c1:empty');
        const pending = deferred<ReturnType<typeof connection>>(); get.mockReturnValueOnce(pending.promise);
        const refresh = client.refetchQueries({ queryKey: ['device-target'] });
        let admitted = false; const admission = current.assertCurrent().then(() => { admitted = true; });
        await act(async () => { await Promise.resolve(); });
        expect(admitted).toBe(false); expect(mounts).toBe(1); expect(unmounts).toBe(0);
        await act(async () => { pending.resolve(connection('c1')); await refresh; await admission; });
        expect(admitted).toBe(true); expect(mounts).toBe(1); expect(unmounts).toBe(0);
        get.mockRejectedValueOnce(Object.assign(new Error('Redis unavailable'), { code: 109 }));
        await act(async () => { await client.refetchQueries({ queryKey: ['device-target'] }); });
        expect(screen.getByText('c1:empty')).toBeInTheDocument();
        await expect(current.assertCurrent()).rejects.toThrow();
        expect(mounts).toBe(1); expect(unmounts).toBe(0);
    });

    it('coalesces simultaneous refresh triggers into the same request', async () => {
        get.mockResolvedValueOnce(connection('c1')); const client = new QueryClient(); renderTarget(client);
        await screen.findByText('c1:empty');
        const pending = deferred<ReturnType<typeof connection>>(); get.mockReturnValue(pending.promise);
        const first = current.refresh(); const second = current.refresh();
        expect(get).toHaveBeenCalledTimes(2);
        expect(get.mock.calls[1][2].signal.aborted).toBe(false);
        await act(async () => { pending.resolve(connection('c1')); await Promise.all([first, second]); });
        expect(mounts).toBe(1); expect(unmounts).toBe(0);
    });

    it('isolates C1 state and pending admission when C2 is discovered', async () => {
        get.mockResolvedValueOnce(connection('c1')); const client = new QueryClient(); renderTarget(client);
        fireEvent.click(await screen.findByText('c1:empty')); await screen.findByText('c1:old-command');
        const pending = deferred<ReturnType<typeof connection>>(); get.mockReturnValueOnce(pending.promise);
        const refresh = client.refetchQueries({ queryKey: ['device-target'] });
        const admission = current.assertCurrent(); const rejected = expect(admission).rejects.toThrow();
        await act(async () => { pending.resolve(connection('c2')); await refresh; await rejected; });
        expect(await screen.findByText('c2:empty')).toBeInTheDocument();
        expect(mounts).toBe(2); expect(unmounts).toBe(1);
    });

    it('removes a runtime on explicit authorization denial', async () => {
        get.mockResolvedValueOnce(connection('c1')); const client = new QueryClient(); renderTarget(client);
        await screen.findByText('c1:empty');
        get.mockRejectedValueOnce(Object.assign(new Error('Device unavailable'), { code: 20 }));
        await act(async () => { await client.refetchQueries({ queryKey: ['device-target'] }); });
        await screen.findByRole('alert'); expect(unmounts).toBe(1);
    });

    it('keys organization and account separately and sends explicit organization', async () => {
        get.mockResolvedValue(connection('c1')); const client = new QueryClient();
        client.setQueryData(['device-target','u:other',5,'d1'], connection('foreign'));
        renderTarget(client, 'u:1', 5); await screen.findByText('c1:empty');
        expect(get.mock.calls[0][1]).toEqual({ scope: 'org', org_id: '5' });
        expect(client.getQueryData(['device-target','u:other',5,'d1'])).toEqual(connection('foreign'));
    });

    it('encodes stable and finite identities without route aliases', () => {
        expect(devicePath('a/b', 'files', 5)).toBe('/desk/device/a%2Fb/files?org_id=5');
        expect(sessionPath('c/1')).toBe('/desk/session/c%2F1');
    });
});

describe('finite session boundary', () => {
    it('uses no device discovery and rejects a grant from another account', async () => {
        saveSessionGrant('c1', { principalKey: 'u:1', grantSessionId: 'secret', accessCeiling: null, source: 'device-code' });
        render(<MemoryRouter initialEntries={['/desk/session/c1']}><Routes><Route path="/desk/session/:connectionId"
            element={<FiniteTargetBoundary accountKey="u:2"><Runtime /></FiniteTargetBoundary>} /></Routes></MemoryRouter>);
        await screen.findByRole('alert'); expect(mounts).toBe(0); expect(get).not.toHaveBeenCalled();
        expect(screen.getAllByRole('link').map(a => a.getAttribute('href'))).toEqual(['/user/login?tab=device_code']);
    });
    it('keeps a fixed connection and no persistent device key', async () => {
        saveSessionGrant('c1', { principalKey: 'code-session', grantSessionId: 'secret', accessCeiling: null, source: 'support' });
        render(<MemoryRouter initialEntries={['/desk/session/c1']}><Routes><Route path="/desk/session/:connectionId"
            element={<FiniteTargetBoundary accountKey="code-session"><Runtime /></FiniteTargetBoundary>} /></Routes></MemoryRouter>);
        await screen.findByText('c1:empty'); expect(current.kind).toBe('session');
        expect(current.resolution.deviceKey).toBeNull(); expect(current.deviceId).toBeUndefined();
        await current.assertCurrent(); expect(get).not.toHaveBeenCalled();
    });
});
