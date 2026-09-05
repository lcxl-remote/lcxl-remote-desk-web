import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { PermissionRequestDto } from '@/services/types';
import { AssistantPermissionRecords } from './assistant-permission-records';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const request = (requestId: string, state: PermissionRequestDto['state']) => ({ requestId, state } as PermissionRequestDto);
const row = (value: PermissionRequestDto) => <div key={value.requestId}>{value.requestId}</div>;

describe('permission request history', () => {
    it('hides resolved rows regardless of history length and opens them on demand', () => {
        const requests = Array.from({ length: 100 }, (_, i) => request(`resolved-${i}`, 'approved'));
        render(<AssistantPermissionRecords requests={requests}>{row}</AssistantPermissionRecords>);
        expect(screen.queryByText('resolved-0')).toBeNull();
        expect(screen.queryByTestId('device-assistant-permission-requests')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.permissionHistory' }));
        expect(screen.getByText('resolved-99')).toBeTruthy();
    });

    it('keeps pending requests visible and moves resolved ones without opening history', () => {
        const { rerender } = render(<AssistantPermissionRecords requests={[request('pending-one', 'pending'), request('review-one', 'needs_revalidation')]}>{row}</AssistantPermissionRecords>);
        expect(screen.getByText('pending-one')).toBeTruthy();
        expect(screen.getByText('review-one')).toBeTruthy();
        rerender(<AssistantPermissionRecords requests={[request('pending-one', 'partially_approved')]}>{row}</AssistantPermissionRecords>);
        expect(screen.queryByText('pending-one')).toBeNull();
        expect(screen.queryByRole('dialog')).toBeNull();
    });

    it('closes records when remounted for a different conversation', () => {
        const { rerender } = render(<AssistantPermissionRecords key="a" requests={[request('old', 'denied')]}>{row}</AssistantPermissionRecords>);
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.permissionHistory' }));
        rerender(<AssistantPermissionRecords key="b" requests={[]}>{row}</AssistantPermissionRecords>);
        expect(screen.queryByRole('dialog')).toBeNull();
        expect(screen.queryByText('old')).toBeNull();
    });
});
