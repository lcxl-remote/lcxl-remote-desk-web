import { fireEvent, render, screen } from '@testing-library/react';
import { useState } from 'react';
import { describe, expect, it, vi } from 'vitest';
import type { PermissionRequestDto } from '@/services/types';
import { AssistantPermissionRecords } from './assistant-permission-records';
import { AssistantComposerTools } from './assistant-composer-tools';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const request = (requestId: string, state: PermissionRequestDto['state']) => ({ requestId, state, items: [] } as unknown as PermissionRequestDto);
const row = (value: PermissionRequestDto) => <div key={value.requestId}>{value.requestId}</div>;
function Records({ requests, automaticApproval = false }: { requests: PermissionRequestDto[]; automaticApproval?: boolean }) {
    const [open, setOpen] = useState(false);
    return <>
        <AssistantPermissionRecords requests={requests} automaticApproval={automaticApproval} open={open} onOpenChange={setOpen}>{row}</AssistantPermissionRecords>
        <AssistantComposerTools meter={<span>meter</span>} onDetails={() => {}} onPermissionHistory={() => setOpen(true)} />
    </>;
}

describe('permission request history', () => {
    it('hides resolved rows regardless of history length and opens them on demand', () => {
        const requests = Array.from({ length: 100 }, (_, i) => request(`resolved-${i}`, 'approved'));
        render(<Records requests={requests} />);
        expect(screen.queryByText('resolved-0')).toBeNull();
        expect(screen.queryByTestId('ai-assistant-permission-requests')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.permissionHistory' }));
        expect(screen.getByText('resolved-99')).toBeTruthy();
    });

    it('keeps pending requests visible and moves resolved ones without opening history', () => {
        const { rerender } = render(<Records requests={[request('pending-one', 'pending'), request('review-one', 'needs_revalidation')]} />);
        expect(screen.getByText('pending-one')).toBeTruthy();
        expect(screen.getByText('review-one')).toBeTruthy();
        rerender(<Records requests={[request('pending-one', 'partially_approved')]} />);
        expect(screen.queryByText('pending-one')).toBeNull();
        expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('replaces manual cards with review status and restores them when automatic approval is disabled', () => {
        const requests = [request('automatic', 'pending')];
        const { rerender } = render(<Records requests={requests} automaticApproval />);
        expect(screen.getByRole('status')).toHaveTextContent('pages.aiAssistant.permissionAutomaticReview');
        expect(screen.queryByText('automatic')).toBeNull();
        expect(screen.queryByTestId('ai-assistant-permission-requests')).toBeNull();
        rerender(<Records requests={requests} />);
        expect(screen.queryByRole('status')).toBeNull();
        expect(screen.getByText('automatic')).toBeTruthy();
        rerender(<Records requests={[request('automatic', 'approved')]} automaticApproval />);
        expect(screen.queryByRole('status')).toBeNull();
        expect(screen.queryByText('automatic')).toBeNull();
    });

    it('keeps interactive commands and requests needing revalidation available for owner review', () => {
        const interactive = { ...request('interactive', 'pending'), items: [{ commandConfirmation: { interactive: {} } }] } as PermissionRequestDto;
        render(<Records requests={[request('automatic', 'pending'), interactive, request('revalidate', 'needs_revalidation')]} automaticApproval />);
        expect(screen.queryByText('automatic')).toBeNull();
        expect(screen.getByText('interactive')).toBeTruthy();
        expect(screen.getByText('revalidate')).toBeTruthy();
    });

    it('keeps the latest reviewer fault visible and opens its durable decision without implying execution', () => {
        const failed = { ...request('failed-review', 'denied'), decision: { source: 'review_unavailable', items: [] } } as PermissionRequestDto;
        const { rerender } = render(<Records requests={[failed]} automaticApproval />);
        expect(screen.getByRole('alert')).toHaveTextContent('pages.aiAssistant.permissionDecisionSource.review_unavailable');
        expect(screen.queryByText('failed-review')).toBeNull();
        fireEvent.click(screen.getAllByRole('button', { name: 'pages.aiAssistant.permissionHistory' })[0]);
        expect(screen.getByText('failed-review')).toBeTruthy();
        rerender(<Records key="after" requests={[failed, request('new-review', 'pending')]} automaticApproval />);
        expect(screen.queryByRole('alert')).toBeNull();
        expect(screen.getByRole('status')).toBeTruthy();
    });

    it('does not present a reasoned AI denial or an owner denial as a reviewer fault', () => {
        const denied = { ...request('valid-denial', 'denied'), decision: { source: 'ai_approval', items: [] } } as PermissionRequestDto;
        render(<Records requests={[denied]} automaticApproval />);
        expect(screen.queryByRole('alert')).toBeNull();
    });

    it('closes records when remounted for a different conversation', () => {
        const { rerender } = render(<Records key="a" requests={[request('old', 'denied')]} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.permissionHistory' }));
        rerender(<Records key="b" requests={[]} />);
        expect(screen.queryByRole('dialog')).toBeNull();
        expect(screen.queryByText('old')).toBeNull();
    });
});
