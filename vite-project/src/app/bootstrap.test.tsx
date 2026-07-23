import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { BootstrapError, bootstrapApplication } from './bootstrap';

afterEach(() => {
    vi.restoreAllMocks();
});

describe('bootstrapApplication', () => {
    it('renders the application after initialization succeeds', async () => {
        const root = { render: vi.fn() };
        const application = <div>application</div>;

        await bootstrapApplication({
            application,
            initialize: vi.fn().mockResolvedValue(undefined),
            root,
        });

        expect(root.render).toHaveBeenCalledOnce();
        expect(root.render).toHaveBeenCalledWith(application);
    });

    it('renders a reload fallback when initialization fails', async () => {
        const root = { render: vi.fn() };
        const error = new Error('locale chunk failed');
        const consoleError = vi
            .spyOn(console, 'error')
            .mockImplementation(() => undefined);

        await bootstrapApplication({
            application: <div>application</div>,
            initialize: vi.fn().mockRejectedValue(error),
            root,
        });

        expect(consoleError).toHaveBeenCalledWith(
            'Application bootstrap failed',
            error,
        );
        expect(root.render).toHaveBeenCalledOnce();

        render(root.render.mock.calls[0][0]);
        expect(screen.getByRole('alert')).toBeInTheDocument();
        expect(
            screen.getByRole('button', { name: /reload/i }),
        ).toBeInTheDocument();
    });
});

describe('BootstrapError', () => {
    it('reloads the page when requested', () => {
        const onReload = vi.fn();
        render(<BootstrapError onReload={onReload} />);

        fireEvent.click(screen.getByRole('button', { name: /reload/i }));

        expect(onReload).toHaveBeenCalledOnce();
    });
});
