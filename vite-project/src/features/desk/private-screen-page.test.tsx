import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import PrivateScreenPage from './private-screen-page';

// i18n: real en-US locale so assertions match the copy users actually see.
vi.mock('react-i18next', () =>
    import('@/test-utils/i18n-mock').then((m) => m.reactI18nextMock()),
);

describe('PrivateScreenPage', () => {
    it('paints an opaque background of its own', () => {
        // The overlay is what stands between a remote session and the physical
        // display, so its own root has to be opaque rather than relying on a
        // theme background further up the tree.
        render(<PrivateScreenPage />);
        expect(screen.getByTestId('private-screen-root').className).toContain('bg-slate-950');
    });

    it('loads no remote asset', () => {
        // A background fetched over the network can fail — offline, blocked, or
        // simply slow — and every one of those leaves the real desktop showing.
        const { container } = render(<PrivateScreenPage />);
        // The Tailwind arbitrary-value form `bg-[url('https://…')]` hides the
        // fetch inside a class name, so the classes are checked as well.
        expect(container.innerHTML).not.toMatch(/url\(\s*['"]?https?:/);
        expect(
            container.querySelector('[src^="http"], [href^="http"], [style*="url("]'),
        ).toBeNull();
    });

    it('renders title, description and hotkey hint from i18n', () => {
        render(<PrivateScreenPage />);
        expect(screen.getByText('Remote Desktop Privacy Mode')).toBeInTheDocument();
        expect(
            screen.getByText(/This screen is being controlled remotely/),
        ).toBeInTheDocument();
        expect(screen.getByText('Ctrl + Alt + L')).toBeInTheDocument();
        expect(
            screen.getByText('Press the hotkey above to exit privacy mode'),
        ).toBeInTheDocument();
    });

    it('offers no interactive control', () => {
        // The overlay window is click-through and never becomes key, so any
        // button here would be unreachable and would only suggest otherwise.
        const { container } = render(<PrivateScreenPage />);
        expect(container.querySelectorAll('button')).toHaveLength(0);
        expect(container.querySelectorAll('a')).toHaveLength(0);
    });
});
