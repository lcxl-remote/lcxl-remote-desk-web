import { fireEvent, render, screen, waitFor, cleanup } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { ScheduleProposalCards } from './proposal-card';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const transport = vi.hoisted(() => ({ isConnected: true, subscribe: vi.fn(() => () => {}), sendTracked: vi.fn(), cancelQueued: vi.fn() }));
vi.mock('@/features/desk/use-desk-signaling', () => ({ useDeskSignaling: () => transport }));
vi.mock('./proposal-review', () => ({ ProposalReview: ({ scheduleId, activationDisabled }: { scheduleId: string; activationDisabled: boolean }) => <div data-testid="review">{scheduleId}<button disabled={activationDisabled}>enable</button></div> }));
afterEach(cleanup);
const tool = { callId: 'call', name: 'request_scheduled_task', status: 'ok' as const, argumentsJson: '{}', output: JSON.stringify({ state: 'draft', kind: 'conversation_resume', schedule_id: '12345678-1234-1234-1234-123456789abc' }) };
const base = { deviceId: 'device', connectionId: 'connection' };
it('opens review after settlement, keeps the entry after a follow-up and respects dismissal', async () => {
    const { rerender } = render(<ScheduleProposalCards {...base} tools={[tool]} running />);
    expect(screen.queryByRole('dialog')).toBeNull();
    rerender(<ScheduleProposalCards {...base} tools={[tool]} running={false} />);
    await waitFor(() => expect(screen.getByRole('dialog')).toBeTruthy());
    fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    rerender(<ScheduleProposalCards {...base} tools={[]} />);
    expect(screen.getByText('schedules.proposal.created')).toBeTruthy();
    expect(screen.queryByRole('dialog')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'schedules.proposal.open' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
    expect(transport.sendTracked).not.toHaveBeenCalled();
});
it('ignores malformed and unsuccessful tool results', () => {
    render(<ScheduleProposalCards {...base} tools={[{ ...tool, status: 'failed' }, { ...tool, output: '{}' }]} />);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(screen.queryByText('schedules.proposal.created')).toBeNull();
});

it('opens synchronous conversation review before the active turn settles', async () => {
    const pending = { ...tool, output: JSON.stringify({ ...JSON.parse(tool.output), awaiting_confirmation: true }) };
    render(<ScheduleProposalCards {...base} tools={[pending]} running />);
    await screen.findByRole('dialog');
    expect((screen.getByRole('button', { name: 'enable' }) as HTMLButtonElement).disabled).toBe(false);
});

it('does not let an earlier automation draft block a synchronous conversation review', async () => {
    const freshId = 'aaaaaaaa-1234-1234-1234-123456789abc';
    const fresh = { ...tool, callId: 'fresh', output: JSON.stringify({ state: 'draft', kind: 'fresh_task', schedule_id: freshId }) };
    const pending = { ...tool, output: JSON.stringify({ ...JSON.parse(tool.output), awaiting_confirmation: true }) };
    render(<ScheduleProposalCards {...base} tools={[fresh, pending]} running />);
    await screen.findByRole('dialog');
    expect(screen.getByTestId('review').textContent).toContain(JSON.parse(tool.output).schedule_id);
    expect(screen.getByTestId('review').textContent).not.toContain(freshId);
});
