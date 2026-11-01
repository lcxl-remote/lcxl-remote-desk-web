import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { OutcomeReview } from './outcome-review';
import type { ScheduleClient } from './client';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
afterEach(cleanup);
it('submits the complete review note and preserves it when the server rejects the input', async () => {
    const request = vi.fn().mockResolvedValueOnce({ result: 'task', task: { status: 'paused', revision: 7 } }).mockRejectedValueOnce(new Error('server limit'));
    render(<OutcomeReview client={{ request } as unknown as ScheduleClient} scheduleId="task" runId="run" connected onReload={async () => {}} />);
    const note = '中🙂'.repeat(10000);
    const input = screen.getByRole('textbox');
    expect(input).not.toHaveAttribute('maxLength');
    fireEvent.change(input, { target: { value: note } });
    fireEvent.click(screen.getByRole('button'));
    await waitFor(() => expect(request).toHaveBeenCalledTimes(2));
    expect(request.mock.calls[1][0]).toMatchObject({ operation: 'acknowledge_run_outcome', expected_revision: 7, note });
    expect(await screen.findByRole('alert')).toBeVisible();
    expect(input).toHaveValue(note);
});
