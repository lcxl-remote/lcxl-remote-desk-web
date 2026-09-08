import { useEffect, useState } from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { constrainAssistantPanel, DeskAssistantPanel } from './desk-assistant-panel';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));
vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });

describe('embedded assistant panel', () => {
    it('keeps the workspace and unsent input mounted while hidden', () => {
        const unmount = vi.fn();
        function Workspace() {
            const [value, setValue] = useState('');
            useEffect(() => unmount, []);
            return <input aria-label="Prompt" value={value} onChange={event => setValue(event.target.value)} />;
        }
        const props = { onClose: vi.fn(), onFocus: vi.fn() };
        const view = render(<DeskAssistantPanel {...props} open><Workspace /></DeskAssistantPanel>);
        fireEvent.change(screen.getByLabelText('Prompt'), { target: { value: 'Unsent request' } });
        view.rerender(<DeskAssistantPanel {...props} open={false}><Workspace /></DeskAssistantPanel>);
        expect(screen.queryByRole('region')).not.toBeInTheDocument();
        expect(unmount).not.toHaveBeenCalled();
        view.rerender(<DeskAssistantPanel {...props} open><Workspace /></DeskAssistantPanel>);
        expect(screen.getByLabelText('Prompt')).toHaveValue('Unsent request');
        expect(unmount).not.toHaveBeenCalled();
    });
    it('isolates typing and pointer events while releasing remote keys on focus', () => {
        const remote = vi.fn();
        const release = vi.fn();
        const close = vi.fn();
        render(<div onKeyDown={remote} onKeyUp={remote} onMouseDown={remote} onWheel={remote}>
            <DeskAssistantPanel open onClose={close} onFocus={release}><input aria-label="Prompt" /></DeskAssistantPanel>
        </div>);
        const input = screen.getByLabelText('Prompt');
        fireEvent.focus(input);
        fireEvent.keyDown(input, { key: 'a' });
        fireEvent.keyUp(input, { key: 'a' });
        fireEvent.mouseDown(input);
        fireEvent.wheel(input);
        expect(remote).not.toHaveBeenCalled();
        expect(release).toHaveBeenCalled();
        fireEvent.click(screen.getByRole('button'));
        expect(close).toHaveBeenCalledOnce();
    });
    it('keeps resized or dragged panels inside small and shrinking viewports', () => {
        expect(constrainAssistantPanel({ x: 900, y: -20, width: 480, height: 680 }, { width: 300, height: 220 }))
            .toEqual({ x: 0, y: 0, width: 300, height: 220 });
        expect(constrainAssistantPanel({ x: 900, y: 600, width: 100, height: 100 }, { width: 1000, height: 700 }))
            .toEqual({ x: 680, y: 420, width: 320, height: 280 });
    });
});
