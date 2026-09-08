import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import type { GrantRequestItemDto, PermissionRequestDto } from '@/services/types';
import { AssistantPermissionRequest } from './assistant-permission-request';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
const item = (overrides: Partial<GrantRequestItemDto> = {}): GrantRequestItemDto => ({
    itemId: 'read', providerId: 'desktop.session', toolName: 'inspect_desktop_session',
    reason: 'Inspect selected device', expectedEffect: 'read_device', resourceScope: ['target:a', 'target:b'],
    operationScope: ['observe'], exportDestinations: [], suggestedMaxUses: 2, suggestedTtlSeconds: 300,
    ...overrides,
});
const request = (items = [item()]): PermissionRequestDto => ({
    schemaVersion: 1, requestId: 'permission-1', inputRevision: 1,
    createdAt: '2026-09-06T00:00:00Z', state: 'pending', items,
});
const submit = () => screen.getByRole('button', { name: 'pages.deviceAssistant.permissionSubmitSelection' });
describe('shared permission review', () => {
    it('submits narrowed scope and explicit denial for missing action reviews', () => {
        const onDecide = vi.fn().mockResolvedValue(true);
        const value = request([item(), item({ itemId: 'command', toolName: 'execute_confirmed_command' }),
            item({ itemId: 'file', toolName: 'delete_text_file' }), item({ itemId: 'send', expectedEffect: 'send_external' })]);
        render(<AssistantPermissionRequest request={value} canDecide onDecide={onDecide} />);
        fireEvent.click(screen.getByRole('checkbox', { name: 'target:b' }));
        fireEvent.click(submit());
        expect(onDecide).toHaveBeenCalledWith(value, [
            { itemId: 'read', decision: 'approve', resource_scope: ['target:a'], operation_scope: ['observe'], export_destinations: [], ttl_seconds: 300, max_uses: 2 },
            { itemId: 'command', decision: 'deny' }, { itemId: 'file', decision: 'deny' }, { itemId: 'send', decision: 'deny' },
        ]);
    });
    it('shows the exact external message and limits its approval to one use', () => {
        const onDecide = vi.fn().mockResolvedValue(true);
        const value = request([item({ itemId: 'send', expectedEffect: 'send_external',
            externalSendConfirmation: { accountId: 'reviewed-account', destination: 'reviewed-recipient',
                channel: 'email', bodyPlainText: 'Exact message for review', bodySizeBytes: 24,
                oneShot: true, attachments: [], subject: 'Reviewed subject' },
        })]);
        render(<AssistantPermissionRequest request={value} canDecide onDecide={onDecide} />);
        expect(screen.getByText('reviewed-account')).toBeInTheDocument();
        expect(screen.getByText('reviewed-recipient')).toBeInTheDocument();
        expect(screen.getByText('Exact message for review')).toBeInTheDocument();
        fireEvent.click(submit());
        expect(onDecide.mock.calls[0][1][0]).toMatchObject({ itemId: 'send', decision: 'approve', max_uses: 1 });
    });
    it.each(['disabled', 'busy'] as const)('blocks both approval and denial when %s', flag => {
        const onDecide = vi.fn().mockResolvedValue(true);
        render(<AssistantPermissionRequest request={request()} canDecide {...{ [flag]: true }} onDecide={onDecide} />);
        expect(submit()).toBeDisabled();
        const deny = screen.getByRole('button', { name: 'pages.deviceAssistant.permissionDeny' });
        expect(deny).toBeDisabled();
        fireEvent.click(submit()); fireEvent.click(deny);
        expect(onDecide).not.toHaveBeenCalled();
    });
    it('allows reading completed reviews while decisions are disabled', () => {
        render(<AssistantPermissionRequest request={{ ...request(), state: 'approved' }} canDecide disabled onDecide={vi.fn()} />);
        fireEvent.click(screen.getByRole('button'));
        expect(screen.getByText('Inspect selected device')).toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'pages.deviceAssistant.permissionSubmitSelection' })).not.toBeInTheDocument();
    });
    it('does not offer decision controls on a viewing-only surface', () => {
        render(<AssistantPermissionRequest request={request()} canDecide={false} onDecide={vi.fn()} />);
        expect(screen.getByText('Inspect selected device')).toBeInTheDocument();
        expect(screen.queryByRole('button')).not.toBeInTheDocument();
    });
});
