import { render, screen } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import { AssistantUnknownOutcome } from './assistant-unknown-outcome';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const outcome = { workId: 44, actionRequestId: 'request-uuid', executionId: 'execution-uuid', workKind: 'computer_action' };
it('shows a readable operation type and keeps identifiers in collapsed details', () => {
    const { container } = render(<AssistantUnknownOutcome outcome={outcome}><button>Close reviewed record</button></AssistantUnknownOutcome>);
    expect(screen.getByText('pages.deviceAssistant.unknownOutcome.title.computer')).toBeTruthy();
    expect(screen.getByText('pages.deviceAssistant.unknownOutcome.check.computer')).toBeTruthy();
    const details = container.querySelector('details')!;
    expect(details.open).toBe(false);
    expect(details.contains(screen.getByText('execution-uuid'))).toBe(true);
    expect(details.contains(screen.getByText('request-uuid'))).toBe(true);
    expect(details.contains(screen.getByRole('button'))).toBe(false);
});
it('does not expose an unknown backend type as the user-facing heading', () => {
    render(<AssistantUnknownOutcome outcome={{ ...outcome, workKind: 'future_internal_kind' }} />);
    expect(screen.getByText('pages.deviceAssistant.unknownOutcome.title.other')).toBeTruthy();
    expect(screen.getByText('future_internal_kind').closest('details')).not.toBeNull();
});
