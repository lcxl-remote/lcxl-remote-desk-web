import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { PromptCacheSettings } from './prompt-cache-settings';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

describe('prompt cache configuration', () => {
    it('defaults to the provider and never enables history implicitly', () => {
        render(<PromptCacheSettings value="{}" onChange={vi.fn()} />);
        expect(screen.queryByRole('switch')).toBeNull();
        expect(screen.getByRole('combobox').textContent).toContain('pages.aiModel.cache.default');
    });
    it('preserves thinking options when explicitly opting in to history', () => {
        const onChange = vi.fn();
        render(<PromptCacheSettings value={JSON.stringify({ thinking: { type: 'adaptive' }, prompt_cache: { mode: 'anthropic_explicit' } })} onChange={onChange} />);
        fireEvent.click(screen.getByRole('switch'));
        expect(JSON.parse(onChange.mock.calls[0][0])).toEqual({ thinking: { type: 'adaptive' }, prompt_cache: { mode: 'anthropic_explicit', cache_history: true } });
    });
    it('does not overwrite invalid advanced JSON', () => {
        const onChange = vi.fn();
        const { container } = render(<PromptCacheSettings value="{" onChange={onChange} />);
        expect(container.textContent).toBe('');
        expect(onChange).not.toHaveBeenCalled();
    });
});
