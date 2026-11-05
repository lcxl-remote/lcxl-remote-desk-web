import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { selectOption } from '@/test-utils/select-option';
import { AssistantApprovalModePicker, subscriptionApprovalMode } from './assistant-approval-mode';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
const api = vi.hoisted(() => ({ load: vi.fn(), save: vi.fn() }));
vi.mock('@/services/clients', () => ({
    getAiAssistantApprovalMode: api.load,
    updateAiAssistantApprovalMode: api.save,
}));

it('requires a separate owner confirmation and leaves cancel focused', async () => {
    api.load.mockResolvedValue({ success: true, data: { mode: 'ai_review', revision: 3, aiReviewAvailable: true } });
    api.save.mockResolvedValue({ success: true, data: { mode: 'no_review', revision: 4, aiReviewAvailable: true } });
    render(<QueryClientProvider client={new QueryClient()}><AssistantApprovalModePicker session="workbench-root" /></QueryClientProvider>);
    const mode = await screen.findByRole('combobox', { name: 'Approval mode' });
    await selectOption(mode, 'No review');
    const warning = within(await screen.findByRole('alertdialog'));
    expect(warning.getByText(/Misleading instructions in web pages, issues, files, or logs/)).toBeInTheDocument();
    const cancel = warning.getByRole('button', { name: 'Cancel' });
    await waitFor(() => expect(cancel).toHaveFocus());
    expect(api.save).not.toHaveBeenCalled();
    fireEvent.click(cancel);
    expect(mode).toHaveTextContent('AI review');
    expect(api.save).not.toHaveBeenCalled();

    await selectOption(mode, 'No review');
    fireEvent.click(await screen.findByRole('button', { name: 'Enable no review' }));
    await waitFor(() => expect(mode).toHaveTextContent('No review'));
    expect(api.save).toHaveBeenCalledExactlyOnceWith({
        session: 'workbench-root', mode: 'no_review', expectedRevision: 3, noReviewConfirmationId: expect.any(String),
    });
});

it('projects a reviewed mode for subscriptions when the root uses no review', () => {
    expect(subscriptionApprovalMode({ mode: 'no_review', revision: 2, aiReviewAvailable: true })).toBe('ai_review');
    expect(subscriptionApprovalMode({ mode: 'no_review', revision: 2, aiReviewAvailable: false })).toBe('manual');
    expect(subscriptionApprovalMode({ mode: 'manual', revision: 3, aiReviewAvailable: true })).toBe('manual');
});
