import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AssistantBrowserPairing } from './assistant-browser-pairing';

const pairing = vi.hoisted(() => ({
    data: undefined as undefined | { data: { pairing_code: string; bridge_url: string; extension_version: string } },
    mutate: vi.fn(), reset: vi.fn(), isPending: false, isError: false,
}));
vi.mock('@/services/hooks/browserExtensionController/useCreateBrowserExtensionPairing', () => ({
    useCreateBrowserExtensionPairing: () => pairing,
}));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

beforeEach(() => {
    vi.clearAllMocks();
    pairing.data = undefined;
    vi.stubGlobal('location', { hostname: 'localhost' });
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); });

describe('local browser pairing', () => {
    it('clears the single-use proof immediately after submission', () => {
        render(<AssistantBrowserPairing assistantEnabled />);
        const input = screen.getByLabelText('pages.aiAssistant.browserExtensionLocalProof');
        fireEvent.change(input, { target: { value: ' local-proof ' } });
        fireEvent.click(screen.getByText('pages.aiAssistant.browserExtensionShowCode'));
        expect(pairing.mutate).toHaveBeenCalledWith({ data: { local_proof: 'local-proof' } });
        expect((input as HTMLInputElement).value).toBe('');
    });

    it('does not offer pairing from a remote page', () => {
        vi.stubGlobal('location', { hostname: 'remote.example' });
        render(<AssistantBrowserPairing assistantEnabled />);
        expect(screen.queryByTestId('browser-extension-pairing')).toBeNull();
        expect(pairing.mutate).not.toHaveBeenCalled();
    });

    it('resets the displayed secret manually and after two minutes', () => {
        vi.useFakeTimers();
        pairing.data = { data: { pairing_code: 'secret', bridge_url: 'ws://127.0.0.1:9000', extension_version: '2' } };
        render(<AssistantBrowserPairing assistantEnabled />);
        fireEvent.click(screen.getByText('pages.aiAssistant.browserExtensionHideCode'));
        expect(pairing.reset).toHaveBeenCalledOnce();
        act(() => { vi.advanceTimersByTime(120_000); });
        expect(pairing.reset).toHaveBeenCalledTimes(2);
    });
});
