import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { useEffect } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { DeviceTargetBoundary, useDeviceTarget, type DeviceTarget } from './device-target-context';
import { useDeskSignaling } from './use-desk-signaling';
import type { ConnectionModel } from '@/services/types';
const get = vi.hoisted(() => vi.fn());
vi.mock('@/services/clients', () => ({ getDeviceConnection: get }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
class Socket {
    static readonly CONNECTING=0; static readonly OPEN=1; static readonly CLOSED=3;
    static instances: Socket[]=[];
    readyState=0; sent: string[]=[]; onopen: ((event:Event)=>void)|null=null;
    onclose: ((event:Event)=>void)|null=null; onerror=null; onmessage=null;
    constructor(readonly url: string) { Socket.instances.push(this); }
    send(data:string) { this.sent.push(data); }
    close() { this.readyState=3; }
}
let current: DeviceTarget;
function Main() {
    const target=useDeviceTarget(); current=target;
    const transport=useDeskSignaling({ usePrepared: true });
    useEffect(() => {
        if (transport.isConnected) transport.sendMessage(10000,{authorizedTarget:target.connectionId},target.connectionId);
    },[transport.isConnected,transport.sendMessage,target.connectionId]);
    return <p>{target.connectionId}:{String(transport.isConnected)}</p>;
}
afterEach(() => { cleanup(); vi.unstubAllGlobals(); get.mockReset(); Socket.instances=[]; });
it('overlaps discovery with one unadmitted socket, then adopts an already-open socket without another handshake', async () => {
    vi.stubGlobal('WebSocket',Socket);
    let resolve!: (value:unknown)=>void;
    get.mockImplementation(() => new Promise(done => { resolve=done; }));
    const client=new QueryClient();
    client.setQueryData(['device-target','u:1',null,'device'],{success:true,code:0,data:{connection_id:'old'} });
    const view=render(<QueryClientProvider client={client}><MemoryRouter initialEntries={['/desk/device/device/control']}>
        <Routes><Route path="/desk/device/:deviceId/control" element={<DeviceTargetBoundary accountKey="u:1"><Main /></DeviceTargetBoundary>} /></Routes>
    </MemoryRouter></QueryClientProvider>);
    await waitFor(() => expect(Socket.instances).toHaveLength(1));
    const socket=Socket.instances[0];
    expect(new URL(socket.url).searchParams.has('token')).toBe(false);
    expect(socket.sent).toHaveLength(0); expect(screen.queryByText(/old/)).not.toBeInTheDocument();
    // The upgrade completes while discovery is pending; no host request is sent.
    socket.readyState=Socket.OPEN;
    await act(async () => resolve({success:true,code:0,data:{connection_id:'c2',device_id:'device'} as ConnectionModel}));
    await screen.findByText('c2:true');
    expect(Socket.instances).toHaveLength(1);
    expect(socket.sent.map(raw => JSON.parse(raw).to_connection_id)).toEqual(['c2']);
    view.unmount(); expect(socket.readyState).toBe(Socket.CLOSED);
});

it('resumes a missing transport after discovery recovers and preserves an established socket during refresh', async () => {
    vi.stubGlobal('WebSocket',Socket);
    get.mockResolvedValue({success:true,code:0,data:{connection_id:'c1',device_id:'device'}});
    const client=new QueryClient();
    render(<QueryClientProvider client={client}><MemoryRouter initialEntries={['/desk/device/device/control']}>
        <Routes><Route path="/desk/device/:deviceId/control" element={<DeviceTargetBoundary accountKey="u:1"><Main /></DeviceTargetBoundary>} /></Routes>
    </MemoryRouter></QueryClientProvider>);
    await screen.findByText('c1:false');
    await waitFor(() => expect(Socket.instances[0].onopen).not.toBeNull());
    const first=Socket.instances[0];
    await act(async () => { first.readyState=Socket.OPEN; first.onopen?.(new Event('open')); });
    await screen.findByText('c1:true');
    await act(async () => { await current.refresh(); });
    expect(Socket.instances).toHaveLength(1);
    get.mockRejectedValue(Object.assign(new Error('Redis unavailable'),{code:109}));
    await act(async () => { first.readyState=Socket.CLOSED; first.onclose?.(new Event('close')); });
    await waitFor(() => expect(client.getQueryState(['device-target','u:1',null,'device'])?.status).toBe('error'));
    // A successful later discovery restarts only the transport. It does not
    // depend on a close event from a socket that was never created.
    get.mockResolvedValue({success:true,code:0,data:{connection_id:'c1',device_id:'device'}});
    await act(async () => { await current.refresh(); });
    await waitFor(() => expect(Socket.instances).toHaveLength(2));
    const second=Socket.instances[1];
    expect(second.sent).toHaveLength(0);
    await act(async () => { second.readyState=Socket.OPEN; second.onopen?.(new Event('open')); });
    await screen.findByText('c1:true');
    await act(async () => { await current.refresh(); });
    expect(Socket.instances).toHaveLength(2);
});
