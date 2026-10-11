import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { createRef, type ComponentProps } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantComposerInput } from './assistant-composer-input';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(module => module.reactI18nextMock()));

const storageKey = 'ai-assistant-send-shortcut';
const hints = {
    enter: 'Enter to send · Shift+Enter for a new line',
    'shift-enter': 'Shift+Enter to send · Enter for a new line',
    'ctrl-enter': 'Ctrl/⌘+Enter to send · Enter for a new line',
};

function mount(props: Partial<ComponentProps<typeof AssistantComposerInput>> = {}) {
    const onSubmit = vi.fn();
    const ref = createRef<HTMLTextAreaElement>();
    const view = render(<form onSubmit={event => { event.preventDefault(); onSubmit(); }}>
        <AssistantComposerInput ref={ref} aria-label="Message" defaultValue="Review this device"
            sendDisabled={false} {...props} />
        <button type="submit" disabled={props.sendDisabled}>Send</button>
    </form>);
    return { ...view, onSubmit, ref, input: within(view.container).getByRole('textbox', { name: 'Message' }) };
}

async function chooseShortcut(value: keyof typeof hints) {
    fireEvent.keyDown(screen.getAllByRole('button', { name: 'Send shortcut' })[0], { key: 'Enter' });
    fireEvent.click(await screen.findByRole('menuitemradio', { name: hints[value] }));
    await waitFor(() => expect(screen.queryByRole('menu')).toBeNull());
}

beforeEach(() => localStorage.removeItem(storageKey));
afterEach(() => { cleanup(); vi.restoreAllMocks(); localStorage.removeItem(storageKey); });

describe('assistant send shortcuts', () => {
    it('defaults to Enter, keeps Shift+Enter available for newlines and submits through the same form as the button', () => {
        const { input, onSubmit, ref, getByRole } = mount();
        expect(ref.current).toBe(input);
        expect(input).toHaveAccessibleDescription(hints.enter);
        expect(fireEvent.keyDown(input, { key: 'Enter', shiftKey: true })).toBe(true);
        expect(onSubmit).not.toHaveBeenCalled();
        expect(fireEvent.keyDown(input, { key: 'Enter' })).toBe(false);
        expect(onSubmit).toHaveBeenCalledOnce();
        fireEvent.click(getByRole('button', { name: 'Send', exact: true }));
        expect(onSubmit).toHaveBeenCalledTimes(2);
    });

    it('saves Shift+Enter through the settings menu and restores it after remounting', async () => {
        const view = mount();
        await chooseShortcut('shift-enter');
        expect(localStorage.getItem(storageKey)).toBe('shift-enter');
        expect(view.input).toHaveAccessibleDescription(hints['shift-enter']);
        expect(fireEvent.keyDown(view.input, { key: 'Enter' })).toBe(true);
        expect(view.onSubmit).not.toHaveBeenCalled();
        expect(fireEvent.keyDown(view.input, { key: 'Enter', shiftKey: true })).toBe(false);
        expect(view.onSubmit).toHaveBeenCalledOnce();
        view.unmount();
        const reopened = mount();
        expect(reopened.input).toHaveAccessibleDescription(hints['shift-enter']);
        fireEvent.keyDown(reopened.input, { key: 'Enter', shiftKey: true });
        expect(reopened.onSubmit).toHaveBeenCalledOnce();
    });

    it('supports Ctrl+Enter and Command+Enter without submitting unmodified Enter or extra modifiers', async () => {
        const { input, onSubmit } = mount();
        await chooseShortcut('ctrl-enter');
        for (const keys of [{}, { shiftKey: true }, { ctrlKey: true, shiftKey: true }, { metaKey: true, altKey: true }]) {
            expect(fireEvent.keyDown(input, { key: 'Enter', ...keys })).toBe(true);
        }
        expect(onSubmit).not.toHaveBeenCalled();
        for (const keys of [{ ctrlKey: true }, { metaKey: true }]) {
            expect(fireEvent.keyDown(input, { key: 'Enter', ...keys })).toBe(false);
        }
        expect(onSubmit).toHaveBeenCalledTimes(2);
    });

    it('leaves unrelated keys and modifier combinations available for editing', () => {
        const { input, onSubmit } = mount();
        for (const keys of [{ key: 'a' }, { key: 'Enter', altKey: true }, { key: 'Enter', ctrlKey: true },
            { key: 'Enter', metaKey: true }, { key: 'Enter', shiftKey: true, ctrlKey: true }]) {
            expect(fireEvent.keyDown(input, keys)).toBe(true);
        }
        expect(onSubmit).not.toHaveBeenCalled();
    });

    it.each(['enter', 'shift-enter', 'ctrl-enter'] as const)('does not submit IME confirmation or held keys in %s mode', mode => {
        localStorage.setItem(storageKey, mode);
        const { input, onSubmit } = mount();
        const keys = { key: 'Enter', shiftKey: mode === 'shift-enter', ctrlKey: mode === 'ctrl-enter' };
        expect(fireEvent.keyDown(input, { ...keys, isComposing: true })).toBe(true);
        expect(fireEvent.keyDown(input, { ...keys, keyCode: 229 })).toBe(true);
        fireEvent.compositionStart(input);
        expect(fireEvent.keyDown(input, keys)).toBe(true);
        fireEvent.compositionEnd(input);
        expect(fireEvent.keyDown(input, { ...keys, repeat: true })).toBe(false);
        expect(onSubmit).not.toHaveBeenCalled();
        fireEvent.keyDown(input, keys);
        expect(onSubmit).toHaveBeenCalledOnce();
    });

    it.each([{ sendDisabled: true }, { disabled: true }])('keeps unavailable input from submitting: %j', props => {
        const { input, onSubmit } = mount(props);
        fireEvent.keyDown(input, { key: 'Enter' });
        expect(onSubmit).not.toHaveBeenCalled();
    });

    it('can submit a fixed rehearsal prompt when sending is available', () => {
        const { input, onSubmit } = mount({ readOnly: true });
        fireEvent.keyDown(input, { key: 'Enter' });
        expect(onSubmit).toHaveBeenCalledOnce();
    });

    it('shares changes with other composers on the page and listens for changes from another tab', async () => {
        const first = mount();
        const second = mount();
        await chooseShortcut('shift-enter');
        expect(first.input).toHaveAccessibleDescription(hints['shift-enter']);
        expect(second.input).toHaveAccessibleDescription(hints['shift-enter']);
        act(() => {
            localStorage.setItem(storageKey, 'ctrl-enter');
            window.dispatchEvent(new StorageEvent('storage', { key: storageKey }));
        });
        expect(second.input).toHaveAccessibleDescription(hints['ctrl-enter']);
        act(() => {
            localStorage.removeItem(storageKey);
            window.dispatchEvent(new StorageEvent('storage', { key: null }));
        });
        expect(first.input).toHaveAccessibleDescription(hints.enter);
        expect(second.input).toHaveAccessibleDescription(hints.enter);
    });

    it('uses Enter for an invalid saved preference', () => {
        localStorage.setItem(storageKey, 'invalid');
        const { input, onSubmit } = mount();
        expect(input).toHaveAccessibleDescription(hints.enter);
        fireEvent.keyDown(input, { key: 'Enter' });
        expect(onSubmit).toHaveBeenCalledOnce();
    });

    it('keeps shortcuts usable on the current page when browser storage is unavailable', async () => {
        vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('Storage blocked'); });
        vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('Storage blocked'); });
        const { input, onSubmit } = mount();
        expect(input).toHaveAccessibleDescription(hints.enter);
        await chooseShortcut('shift-enter');
        fireEvent.keyDown(input, { key: 'Enter', shiftKey: true });
        expect(onSubmit).toHaveBeenCalledOnce();
    });

    it('uses the supplied send action outside a form', () => {
        const onSend = vi.fn();
        const view = render(<AssistantComposerInput aria-label="Question" sendDisabled={false} onSend={onSend} />);
        fireEvent.keyDown(view.getByRole('textbox'), { key: 'Enter' });
        expect(onSend).toHaveBeenCalledOnce();
    });
});
