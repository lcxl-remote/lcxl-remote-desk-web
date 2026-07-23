import { fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';

vi.mock('react-i18next', () => ({
    useTranslation: () => ({
        t: (key: string) => key,
    }),
}));

vi.mock('@/hooks/use-device-id', () => ({
    useDeviceId: () => 'device-1',
}));

vi.mock('@/services/hooks/terminalController/useListTerminal', () => ({
    useListTerminal: () => ({
        data: {
            commands: [['pwsh', '-NoLogo']],
        },
        isLoading: false,
    }),
}));

vi.mock('./terminal-session', () => ({
    TerminalView: ({ command }: { command: string }) => (
        <div data-testid="terminal-runtime">{command}</div>
    ),
}));

import TerminalSessionLauncher from './terminal-session-launcher';

describe('TerminalSessionLauncher', () => {
    it('loads the terminal runtime only after selecting a shell', async () => {
        render(
            <MemoryRouter initialEntries={['/desk/connection-1/terminal']}>
                <Routes>
                    <Route
                        path="/desk/:id/terminal"
                        element={<TerminalSessionLauncher />}
                    />
                </Routes>
            </MemoryRouter>,
        );

        expect(screen.queryByTestId('terminal-runtime')).not.toBeInTheDocument();

        fireEvent.change(screen.getByRole('combobox'), {
            target: { value: 'pwsh,-NoLogo' },
        });

        expect(await screen.findByTestId('terminal-runtime')).toHaveTextContent(
            'pwsh,-NoLogo',
        );
    });
});
