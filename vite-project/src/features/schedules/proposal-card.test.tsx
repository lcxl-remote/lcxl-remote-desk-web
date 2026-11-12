import { render, screen, waitFor, cleanup } from '@testing-library/react';
import { useEffect } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { ScheduleProposalCards } from './proposal-card';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const transport = vi.hoisted(() => ({ isConnected: true, subscribe: vi.fn(() => () => {}), sendTracked: vi.fn(), cancelQueued: vi.fn() }));
vi.mock('@/features/desk/use-desk-signaling', () => ({ useDeskSignaling: () => transport }));
const status = vi.hoisted(() => ({ value: 'pending_review' }));
vi.mock('./proposal-review', () => ({ ProposalReview: ({ scheduleId, onLoaded }: { scheduleId: string; onLoaded?: (task: { schedule_id: string; status: string }) => void }) => {
    useEffect(() => { onLoaded?.({ schedule_id: scheduleId, status: status.value }); }, [scheduleId]);
    return <div data-testid="review">{scheduleId}</div>;
} }));
afterEach(() => { cleanup(); vi.clearAllMocks(); status.value = 'pending_review'; });
const id = '12345678-1234-1234-1234-123456789abc';
const tool = { callId: 'call', name: 'request_scheduled_task', status: 'ok' as const, argumentsJson: '{}',
    output: JSON.stringify({ state: 'pending_review', kind: 'fresh_task', awaiting_decision: true, schedule_id: id }) };
const base = { deviceId: 'device', connectionId: 'connection' };

it('renders a waiting fresh-task proposal inline for a decision', async () => {
    render(<ScheduleProposalCards {...base} tools={[tool]} running />);
    expect(await screen.findByTestId('review')).toHaveTextContent(id);
    expect(screen.queryByRole('dialog')).toBeNull();
});

it('ignores enabled conversation timers, malformed and unsuccessful tool results', () => {
    const timer = { ...tool, output: JSON.stringify({ state: 'active', kind: 'conversation_resume', awaiting_decision: false, schedule_id: id }) };
    render(<ScheduleProposalCards {...base} tools={[timer, { ...tool, status: 'failed' }, { ...tool, output: '{}' }]} />);
    expect(screen.queryByTestId('review')).toBeNull();
});

it('reports only proposals whose current state still needs a decision', async () => {
    const changed = vi.fn();
    render(<ScheduleProposalCards {...base} tools={[tool]} onPendingCountChange={changed} />);
    await waitFor(() => expect(changed).toHaveBeenLastCalledWith(1));
    expect(document.querySelectorAll('[data-assistant-pending]')).toHaveLength(1);
});

it('shows the decision once the proposal was approved elsewhere', async () => {
    status.value = 'active';
    render(<ScheduleProposalCards {...base} tools={[tool]} />);
    expect(await screen.findByText('schedules.proposal.approved')).toBeTruthy();
});

it('keeps multiple proposals visible across follow-up renders', async () => {
    const second = { ...tool, callId: 'second', output: JSON.stringify({ ...JSON.parse(tool.output), schedule_id: 'bbbbbbbb-1234-1234-1234-123456789abc' }) };
    const { rerender } = render(<ScheduleProposalCards {...base} tools={[tool, second]} running />);
    await waitFor(() => expect(screen.getAllByTestId('review')).toHaveLength(2));
    rerender(<ScheduleProposalCards {...base} tools={[]} running={false} />);
    expect(screen.getAllByTestId('review')).toHaveLength(2);
});
