import { act, render, screen } from '@testing-library/react';
import { createInstance } from 'i18next';
import { I18nextProvider } from 'react-i18next';
import { describe, expect, it } from 'vitest';
import en from '@/locales/en-US/pages';
import zh from '@/locales/zh-CN/pages';
import { ResolutionStatusToast } from './desk-session-panels';

describe('ResolutionStatusToast', () => {
    it('distinguishes unchanged and applied resolutions with localized messages', async () => {
        const i18n = createInstance();
        await i18n.init({
            lng: 'zh-CN',
            fallbackLng: 'en-US',
            resources: {
                'zh-CN': { translation: zh },
                'en-US': { translation: en },
            },
        });
        const { rerender } = render(
            <I18nextProvider i18n={i18n}>
                <ResolutionStatusToast toast={{ phase: 'unchanged', currentW: 1280, currentH: 800 }} />
            </I18nextProvider>,
        );
        expect(screen.getByTestId('resolution-toast')).toHaveTextContent('无需切换，保持 1280×800');
        expect(screen.getByTestId('resolution-toast')).toHaveAttribute('data-phase', 'unchanged');

        await act(() => i18n.changeLanguage('en-US'));
        expect(screen.getByTestId('resolution-toast')).toHaveTextContent('No change needed; keeping 1280×800');

        rerender(
            <I18nextProvider i18n={i18n}>
                <ResolutionStatusToast toast={{ phase: 'success', appliedW: 1920, appliedH: 1080 }} />
            </I18nextProvider>,
        );
        expect(screen.getByTestId('resolution-toast')).toHaveTextContent('Applied 1920×1080');
        expect(screen.getByTestId('resolution-toast')).toHaveAttribute('data-phase', 'success');
    });
});
