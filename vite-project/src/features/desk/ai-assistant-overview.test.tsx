import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { expect, it, vi } from 'vitest';
import { AiAssistantOverview } from './ai-assistant-overview';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

it('opens conversations, attention, and scheduled tasks from the AI Assistant overview cards', () => {
    render(<MemoryRouter><AiAssistantOverview /></MemoryRouter>);

    expect(screen.getByRole('link', { name: /AI conversations/ })).toHaveAttribute('href', '/ai-assistant/conversations');
    expect(screen.getByRole('link', { name: /AI Assistant attention/ })).toHaveAttribute('href', '/ai-assistant/attention');
    expect(screen.getByRole('link', { name: /Scheduled tasks/ })).toHaveAttribute('href', '/schedules');
});
