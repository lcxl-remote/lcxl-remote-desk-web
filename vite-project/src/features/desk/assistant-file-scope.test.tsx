import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantFileScope, type AssistantFileScopeView } from './assistant-file-scope';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

vi.mock('@/features/file-manager/remote-directory-picker', () => ({ RemoteDirectoryPicker: ({ onSelect }: { onSelect: (path: string) => void }) => <button onClick={() => onSelect('/private/tmp/a directory ')}>choose</button> }));

describe('conversation directories', () => {
    it.each(['target', null])('adds a directory for selected target %s with the observed revision', (target) => {
        const update = vi.fn((..._args: unknown[]) => true);
        render(<AssistantFileScope deskId="device" sessionTargetId={target} scope={{ revision: 7, directories: [] }} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        expect(screen.queryByRole('textbox')).toBeNull();
        const button = screen.getByRole('button', { name: 'pages.aiAssistant.directories.add' });
        expect(button.getAttribute('type')).toBe('button');
        fireEvent.click(button);
        fireEvent.click(screen.getByText("choose"));
        expect(update).toHaveBeenCalledWith({ kind: 'select_directory', path: '/private/tmp/a directory ', purpose: 'pages.aiAssistant.directories.manualPurpose', expected_revision: 7 }, 'pages.aiAssistant.directories.timeout');
    });

    it('blocks unresolved targets and closes the picker when target readiness is lost', () => {
        const props = { deskId: 'device', scope: { revision: 0, directories: [] }, open: true, onOpenChange: vi.fn(), disabled: false, onUpdate: vi.fn() };
        const view = render(<AssistantFileScope {...props} />);
        const add = screen.getByRole('button', { name: 'pages.aiAssistant.directories.add' });
        expect(add).toBeDisabled();
        view.rerender(<AssistantFileScope {...props} sessionTargetId={null} />);
        expect(add).toBeEnabled();
        fireEvent.click(add);
        expect(screen.getByText('choose')).toBeInTheDocument();
        view.rerender(<AssistantFileScope {...props} />);
        expect(add).toBeDisabled();
        expect(screen.queryByText('choose')).toBeNull();
    });

    it('binds approval and revocation to exact directory ids and current revision', () => {
        const update = vi.fn((..._args: unknown[]) => true);
        const scope: AssistantFileScopeView = { revision: 9, directories: [
            { requestId: 'pending', canonicalPath: '/resolved/path', purpose: 'purpose', state: 'pending', source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z' },
            { requestId: 'approved', canonicalPath: '/other/path', purpose: 'purpose', state: 'approved', source: 'owner_selection', referenceExpiresAt: '2030-01-01T00:00:00Z' },
        ] };
        render(<AssistantFileScope scope={scope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.directories.approve' }));
        expect(update.mock.calls[0][0]).toEqual({ kind: 'decide_directory', directory_request_id: 'pending', approve: true, expected_revision: 9 });
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.directories.remove' }));
        expect(update.mock.calls[1][0]).toEqual({ kind: 'revoke_directory', directory_request_id: 'approved', expected_revision: 9 });
    });

    it('does not show directory paths while the panel is closed', () => {
        render(<AssistantFileScope scope={{ revision: 0, directories: [] }} open={false} onOpenChange={() => {}} disabled={false} onUpdate={() => true} />);
        expect(screen.queryByRole('dialog')).toBeNull();
    });
});
