import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantComposerTools } from './assistant-composer-tools';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('assistant composer tools', () => {
    it('orders the meter, details icon and permission history icon and never submits the form', () => {
        const onDetails = vi.fn();
        const onPermissionHistory = vi.fn();
        const onSubmit = vi.fn();
        render(<form onSubmit={onSubmit}><AssistantComposerTools meter={<span data-testid="meter" />} onDetails={onDetails} onPermissionHistory={onPermissionHistory} /></form>);
        const tools = screen.getByTestId('assistant-composer-tools');
        const details = screen.getByRole('button', { name: 'pages.deviceAssistant.workspace.details' });
        const history = screen.getByRole('button', { name: 'pages.deviceAssistant.permissionHistory' });
        expect(Array.from(tools.children)).toEqual([screen.getByTestId('meter'), details, history]);
        for (const button of [details, history]) {
            expect(button.title).toBe(button.getAttribute('aria-label'));
            expect(button.querySelector('.assistant-action-label')?.textContent).toBe(button.getAttribute('aria-label'));
            expect(button.querySelector('svg')).not.toBeNull();
            expect(button.getAttribute('aria-haspopup')).toBe('dialog');
            fireEvent.click(button);
        }
        expect(onDetails).toHaveBeenCalledOnce();
        expect(onPermissionHistory).toHaveBeenCalledOnce();
        expect(onSubmit).not.toHaveBeenCalled();
    });
});

it('places scheduled tasks immediately after directories and opens them without submitting', () => {
    const onSchedules = vi.fn();
    const onSubmit = vi.fn();
    render(<form onSubmit={onSubmit}><AssistantComposerTools meter={null} onDetails={() => {}}
        onPermissionHistory={() => {}} onDirectories={() => {}} onSchedules={onSchedules} /></form>);
    const directories = screen.getByRole('button', { name: 'pages.deviceAssistant.directories.title' });
    const schedules = screen.getByRole('button', { name: 'pages.deviceAssistant.schedules.title' });
    expect(directories.nextElementSibling).toBe(schedules);
    fireEvent.click(schedules);
    expect(onSchedules).toHaveBeenCalledOnce();
    expect(onSubmit).not.toHaveBeenCalled();
});
