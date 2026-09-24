import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { expect, it, vi } from 'vitest';
import { AiAssistantConversationChoices } from './ai-assistant-conversations';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

it('opens the assistant for the selected online device', () => {
    render(<MemoryRouter><AiAssistantConversationChoices
        devices={[{ connectionId: 'desk-1', name: 'Office Mac', description: 'macOS' }]}
        loading={false}
        error={false}
        refreshing={false}
        onRefresh={() => {}}
    /></MemoryRouter>);

    expect(screen.getByText('Office Mac')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: /Open AI Assistant/ }))
        .toHaveAttribute('href', '/desk/desk-1/assistant');
});
