import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantComposerActions } from './assistant-composer-actions';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

const defaults = { turnRunning: false, canStop: false, stopping: false, sendDisabled: false,
    rehearsal: false, onStop: vi.fn() };
const stop = 'pages.aiAssistant.stop';
const send = 'pages.aiAssistant.send';

describe('assistant composer actions', () => {
    it('keeps a visible disabled stop while the first running turn has no durable snapshot', () => {
        const onStop = vi.fn();
        const view = render(<AssistantComposerActions {...defaults} turnRunning onStop={onStop} />);
        const button = view.getByRole('button', { name: stop });
        expect(button).toBeDisabled();
        expect(view.queryByRole('button', { name: send })).toBeNull();
        fireEvent.click(button);
        expect(onStop).not.toHaveBeenCalled();
        view.rerender(<AssistantComposerActions {...defaults} turnRunning canStop onStop={onStop} />);
        expect(view.getByRole('button', { name: stop })).toBeEnabled();
        fireEvent.click(view.getByRole('button', { name: stop }));
        expect(onStop).toHaveBeenCalledOnce();
    });

    it('keeps the stop pending and prevents input submission until stop settles', () => {
        const view = render(<AssistantComposerActions {...defaults} canStop stopping />);
        expect(view.getByRole('button', { name: 'pages.aiAssistant.stopping' })).toBeDisabled();
        expect(view.queryByRole('button', { name: send })).toBeNull();
        view.rerender(<AssistantComposerActions {...defaults} />);
        expect(view.queryByRole('button', { name: stop })).toBeNull();
        expect(view.getByRole('button', { name: send })).toHaveAttribute('type', 'submit');
    });

    it('allows stopping unfinished children while leaving the idle main composer available', () => {
        const onStop = vi.fn(); const onSubmit = vi.fn();
        const view = render(<form onSubmit={onSubmit}>
            <AssistantComposerActions {...defaults} canStop onStop={onStop} />
        </form>);
        fireEvent.click(view.getByRole('button', { name: stop }));
        expect(onStop).toHaveBeenCalledOnce();
        expect(onSubmit).not.toHaveBeenCalled();
        expect(view.getByRole('button', { name: send })).toBeEnabled();
    });

    it('retains input and rehearsal eligibility controls', () => {
        const view = render(<AssistantComposerActions {...defaults} sendDisabled />);
        expect(view.getByRole('button', { name: send })).toBeDisabled();
        view.rerender(<AssistantComposerActions {...defaults} rehearsal sendDisabled />);
        expect(view.getByRole('button', { name: 'schedules.rehearsal.begin' })).toBeDisabled();
    });
});
