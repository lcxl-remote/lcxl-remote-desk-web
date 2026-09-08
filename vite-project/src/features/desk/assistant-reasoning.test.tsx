import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantReasoning } from './assistant-reasoning';
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('assistant reasoning', () => {
    it('is folded by default and can be expanded without changing the text', () => {
        const { container } = render(<AssistantReasoning text="Model supplied reasoning" />);
        const details = container.querySelector('details')!;
        expect(details.open).toBe(false);
        fireEvent.click(screen.getByText('pages.deviceAssistant.reasoning'));
        expect(details.open).toBe(true);
        expect(screen.getByText('Model supplied reasoning')).toBeTruthy();
    });
    it('does not create an empty thinking section', () => {
        const { container, rerender } = render(<AssistantReasoning text={undefined} />);
        expect(container.innerHTML).toBe('');
        rerender(<AssistantReasoning text={' \n'} />);
        expect(container.innerHTML).toBe('');
    });
});
