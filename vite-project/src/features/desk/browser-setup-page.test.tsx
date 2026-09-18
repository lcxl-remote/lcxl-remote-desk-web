import { render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { describe, expect, it, vi } from 'vitest';
import BrowserSetupPage from './browser-setup-page';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

function renderGuide(returnTo?: string) {
    return render(<MemoryRouter initialEntries={[{ pathname: '/desk/device-a/browser-setup', state: { returnTo } }]}>
        <Routes><Route path="/desk/:id/browser-setup" element={<BrowserSetupPage />} /></Routes>
    </MemoryRouter>);
}

describe('browser setup guide navigation', () => {
    it('returns to the original rehearsal and targets the same device for remote control', () => {
        renderGuide('/desk/device-a/assistant?rehearsal=run-1');
        expect(screen.getAllByRole('link', { name: /browserSetup.back/ })[0])
            .toHaveAttribute('href', '/desk/device-a/assistant?rehearsal=run-1');
        expect(screen.getByRole('link', { name: /browserTakeoverAction/ }))
            .toHaveAttribute('href', '/desk/device-a/control');
        expect(screen.getAllByRole('listitem')).toHaveLength(4);
    });

    it.each(['https://example.com', '/desk/other-device/assistant', undefined])(
        'falls back to this AI assistant for an invalid or missing origin: %s', (origin) => {
            renderGuide(origin);
            expect(screen.getAllByRole('link', { name: /browserSetup.back/ })[0])
                .toHaveAttribute('href', '/desk/device-a/assistant');
        },
    );
});
