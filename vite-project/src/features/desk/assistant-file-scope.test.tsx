import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantFileScope, type AssistantFileScopeView } from './assistant-file-scope';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('conversation directories', () => {
    it('adds the exact path with the observed scope revision and never submits a form', () => {
        const update = vi.fn((..._args: unknown[]) => true);
        render(<AssistantFileScope scope={{ revision: 7, directories: [] }} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        fireEvent.change(screen.getByLabelText('pages.deviceAssistant.directories.path'), { target: { value: '/private/tmp/a directory ' } });
        const button = screen.getByRole('button', { name: 'pages.deviceAssistant.directories.add' });
        expect(button.getAttribute('type')).toBe('button');
        fireEvent.click(button);
        expect(update).toHaveBeenCalledWith({ kind: 'select_directory', path: '/private/tmp/a directory ', purpose: 'pages.deviceAssistant.directories.manualPurpose', expected_revision: 7 }, 'pages.deviceAssistant.directories.timeout');
    });

    it('binds approval and revocation to exact directory ids and current revision', () => {
        const update = vi.fn((..._args: unknown[]) => true);
        const scope: AssistantFileScopeView = { revision: 9, directories: [
            { requestId: 'pending', canonicalPath: '/resolved/path', purpose: 'purpose', state: 'pending', source: 'model_proposal', referenceExpiresAt: '2030-01-01T00:00:00Z' },
            { requestId: 'approved', canonicalPath: '/other/path', purpose: 'purpose', state: 'approved', source: 'owner_selection', referenceExpiresAt: '2030-01-01T00:00:00Z' },
        ] };
        render(<AssistantFileScope scope={scope} open onOpenChange={() => {}} disabled={false} onUpdate={update} />);
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.directories.approve' }));
        expect(update.mock.calls[0][0]).toEqual({ kind: 'decide_directory', directory_request_id: 'pending', approve: true, expected_revision: 9 });
        fireEvent.click(screen.getByRole('button', { name: 'pages.deviceAssistant.directories.remove' }));
        expect(update.mock.calls[1][0]).toEqual({ kind: 'revoke_directory', directory_request_id: 'approved', expected_revision: 9 });
    });

    it('does not show directory paths while the panel is closed', () => {
        render(<AssistantFileScope scope={{ revision: 0, directories: [] }} open={false} onOpenChange={() => {}} disabled={false} onUpdate={() => true} />);
        expect(screen.queryByRole('dialog')).toBeNull();
    });
});
