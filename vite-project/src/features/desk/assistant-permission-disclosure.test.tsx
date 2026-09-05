import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantPermissionDisclosure } from './assistant-permission-disclosure';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const card = (state: string) => <AssistantPermissionDisclosure state={state} tools={['execute_confirmed_command']}>
    <div>Full command and scope</div>
</AssistantPermissionDisclosure>;

describe('permission disclosure', () => {
    it.each(['pending', 'needs_revalidation', 'unknown'])('keeps %s requests expanded', (state) => {
        render(card(state));
        expect(screen.getByText('Full command and scope')).toBeTruthy();
        expect(screen.queryByRole('button')).toBeNull();
    });
    it.each(['approved', 'partially_approved', 'denied', 'replaced', 'withdrawn'])('collapses %s requests and allows reviewing them', (state) => {
        render(card(state));
        expect(screen.queryByText('Full command and scope')).toBeNull();
        expect(screen.getByText('execute_confirmed_command')).toBeTruthy();
        fireEvent.click(screen.getByRole('button'));
        expect(screen.getByText('Full command and scope')).toBeTruthy();
        fireEvent.click(screen.getByRole('button'));
        expect(screen.queryByText('Full command and scope')).toBeNull();
    });
    it('collapses after a remote decision and reopens when revalidation is needed', () => {
        const { rerender } = render(card('pending'));
        rerender(card('approved'));
        expect(screen.queryByText('Full command and scope')).toBeNull();
        fireEvent.click(screen.getByRole('button'));
        rerender(card('approved'));
        expect(screen.getByText('Full command and scope')).toBeTruthy();
        rerender(card('needs_revalidation'));
        expect(screen.getByText('Full command and scope')).toBeTruthy();
        rerender(card('denied'));
        expect(screen.queryByText('Full command and scope')).toBeNull();
    });
});
