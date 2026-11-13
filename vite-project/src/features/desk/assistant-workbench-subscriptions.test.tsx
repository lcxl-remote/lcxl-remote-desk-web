import { render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { MemoryRouter } from 'react-router-dom';
import { expect, it, vi } from 'vitest';
import { tMock as t } from '@/test-utils/i18n-mock';
import { WorkbenchSubscriptions } from './assistant-workbench-subscriptions';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));
vi.mock('@/services/clients', async importOriginal => ({
    ...await importOriginal<typeof import('@/services/clients')>(),
    getMyIssueConnections: async () => ({ success: true, data: [] }),
    listAiAssistantIssueSubscriptions: async () => ({ success: true, data: [] }),
}));

function subscriptions(issueConnectionsPath?: string | null) {
    render(<QueryClientProvider client={new QueryClient()}>
        <MemoryRouter>
            <WorkbenchSubscriptions workbench="main" issueConnectionsPath={issueConnectionsPath} onOpenTask={() => {}} />
        </MemoryRouter>
    </QueryClientProvider>);
}

it('links an owner without issue connections to their settings', async () => {
    subscriptions('/system/issue-connections');
    expect(await screen.findByText(t('pages.aiAssistant.issueSources.empty'))).toBeInTheDocument();
    expect(screen.getByRole('button', { name: t('pages.aiAssistant.workbench.newSubscription') })).toBeDisabled();
    expect(screen.getByRole('link', { name: t('pages.issueConnections.title') })).toHaveAttribute('href', '/system/issue-connections');
});

it('omits the settings entry for viewers who cannot manage connections', async () => {
    subscriptions(null);
    expect(await screen.findByText(t('pages.aiAssistant.issueSources.empty'))).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: t('pages.issueConnections.title') })).not.toBeInTheDocument();
});
