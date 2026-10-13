import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { ApprovalReviews } from './reviews';
import { ApprovalMetricsPanel } from './metrics';
import { ApprovalTraceSettingsPanel } from './settings';
import type { ApprovalApi } from './api';
import type { ApprovalReviewDto, ApprovalMetricsOverviewDto } from '@/services/types';
import { deskErrorCodeEnum } from '@/services/types';
import { coverage, summary } from '@/features/model-metrics/test-fixtures';
const state = vi.hoisted(() => ({ access: 'admin', user: '1', target: null as string | null, toast: vi.fn() }));
vi.mock('@/services/hooks/userController/useGetCurrentUser', () => ({ useGetCurrentUser: () => ({ data: { data: { access: state.access, user_id: state.user, target_connection_id: state.target } } }) }));
vi.mock('@/hooks/use-toast', () => ({ useToast: () => ({ toast: state.toast }) }));
vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
const clients: QueryClient[] = [];
afterEach(() => { cleanup(); clients.splice(0).forEach(client => client.clear()); state.access = 'admin'; state.user = '1'; state.target = null; vi.clearAllMocks(); });
function record(): ApprovalReviewDto {
    return { id: 'candidate', session: 'root', root_session: 'root', session_kind: 'main', session_label: null, task: null, source_kind: 'permission_item', source_id: 'source', status: 'denied', reason: null, model_name: 'approval-model', protocol: 'open_ai_chat_completions', model_revision: '12', output_limit: '65536', created_at_ms: 1000, finished_at_ms: 9000, business_finished_at_ms: 9100, stop_reason: 'end_turn', usage: null, recording_state: 'partial', captured_bytes: '123', decision: { verdict: 'deny' }, grant_states: [], execution_state: null, timeline: [] };
}
function api(): ApprovalApi {
    return { list: vi.fn().mockResolvedValue({ reviews: [record()], next_cursor: null }), detail: vi.fn().mockResolvedValue(record()), parts: vi.fn().mockResolvedValue({ parts: [{ sequence: 0, kind: 'input', text: JSON.stringify({ messages: [{ role: 'user', content: '<img src=x onerror=alert(1)>' }] }), at_ms: 1000 }], next_after: null, recording_state: 'partial' }), settings: vi.fn().mockResolvedValue({ revision: '1', enabled: true, retention_days: 7, storage_budget_bytes: '268435456', max_trace_bytes: 16777216 }), save: vi.fn(), metrics: vi.fn() };
}
function mount(element: React.ReactNode) {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } }); clients.push(client);
    const view = render(<QueryClientProvider client={client}>{element}</QueryClientProvider>);
    return { client, ...view, rerender: (element: React.ReactNode) => view.rerender(<QueryClientProvider client={client}>{element}</QueryClientProvider>) };
}
describe('approval details and platform statistics', () => {
    it('does not fetch details for ordinary or scoped users', () => {
        const value = api(); state.access = 'user'; const view = mount(<ApprovalReviews session="root" open api={value}/>);
        expect(value.list).not.toHaveBeenCalled(); state.access = 'admin'; state.target = 'shared-device'; view.rerender(<ApprovalReviews session="root" open api={value}/>);
        expect(value.list).not.toHaveBeenCalled();
    });
    it('renders readable captured text literally and preserves partial and parsed decision labels', async () => {
        const value = api(); const view = mount(<ApprovalReviews session="root" open api={value}/>);
        fireEvent.click(await screen.findByRole('button', { name: /root/ }));
        await screen.findByText('<img src=x onerror=alert(1)>'); expect(view.container.querySelector('img')).toBeNull();
        expect(screen.getByText('Decision in the final review record')).toBeTruthy();
        expect(screen.getByText(/Recording is incomplete/)).toBeTruthy();
        expect(value.list).toHaveBeenCalledWith(expect.objectContaining({ include_children: true, session: 'root' }), expect.anything());
    });
    it('clears detail content when part authorization is revoked and when the account changes', async () => {
        const value = api(); vi.mocked(value.parts).mockRejectedValue({ code: deskErrorCodeEnum.PERMISSION_ERROR });
        const view = mount(<ApprovalReviews session="root" open api={value}/>);
        fireEvent.click(await screen.findByRole('button', { name: /root/ }));
        await screen.findByText(/do not have access/i); expect(screen.queryByText('approval-model')).toBeNull();
        state.user = '2'; vi.mocked(value.list).mockResolvedValue({ reviews: [], next_cursor: null }); view.rerender(<ApprovalReviews session="root" open api={value}/>);
        await waitFor(() => expect(view.client.getQueryCache().getAll().some(query => String(query.queryKey[2]).startsWith('1:'))).toBe(false));
    });
    it('uses separate platform result rates and excludes other results from the denominator', async () => {
        const latency = summary().duration;
        const value: ApprovalMetricsOverviewDto = { rates: { approved: '60', denied: '20', fault: '10', denominator: '90', approval_rate: 60 / 90, denial_rate: 20 / 90, fault_rate: 10 / 90 }, counts: [{ kind: 'pending', count: '2' }, { kind: 'cancelled', count: '5' }, { kind: 'policy_blocked', count: '3' }], timeseries: [], session_kinds: [], waiting: latency, before_model: latency, pending_age: latency, unknown_waiting: '0', as_of_ms: 9000, coverage: coverage() };
        const load = vi.fn().mockResolvedValue(value); mount(<ApprovalMetricsPanel load={load} range={{ from: '2026-10-08T00:00:00Z', to: '2026-10-09T00:00:00Z' }} identity="admin" enabled autoRefresh={false}/>);
        await screen.findByText('66.67%'); expect(screen.getByText('22.22%')).toBeTruthy(); expect(screen.getByText('11.11%')).toBeTruthy();
        expect(load.mock.calls[0][0]).not.toHaveProperty('tool'); expect(load.mock.calls[0][0]).not.toHaveProperty('origin');
    });
    it('refreshes a settings revision conflict and reports it through toast', async () => {
        const value = api(); vi.mocked(value.save).mockRejectedValue({ code: deskErrorCodeEnum.REVISION_CONFLICT });
        mount(<ApprovalTraceSettingsPanel api={value}/>);
        const fields = await screen.findAllByRole('spinbutton'); fireEvent.change(fields[0], { target: { value: '3' } }); fireEvent.click(screen.getByRole('button', { name: 'Save' }));
        await waitFor(() => expect(state.toast).toHaveBeenCalledWith(expect.objectContaining({ variant: 'destructive' })));
        expect(value.settings).toHaveBeenCalledTimes(2);
    });
});
