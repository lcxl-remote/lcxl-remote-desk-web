import { useState } from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { ModelThinkingSettings, updateThinkingOption } from './model-thinking-settings';

vi.mock('react-i18next', () => import('@/test-utils/i18n-mock').then(m => m.reactI18nextMock()));

function Editor({ initial }: { initial: string }) {
    const [value, setValue] = useState(initial);
    return <><ModelThinkingSettings protocol="anthropic_messages" contract="anthropic_messages" prefixBinding={false}
        value={value} onChange={setValue} onContractChange={() => {}} onPrefixBindingChange={() => {}} />
        <output data-testid="options">{value}</output></>;
}

describe('model thinking settings', () => {
    it('edits the same JSON without losing cache options or unrelated thinking settings', () => {
        const initial = JSON.stringify({ prompt_cache: { mode: 'anthropic_explicit', cache_history: false }, thinking: { type: 'adaptive', display: 'omitted' } });
        render(<Editor initial={initial} />);
        fireEvent.click(screen.getByRole('switch', { name: 'Enable provider thinking clearing' }));
        expect(JSON.parse(screen.getByTestId('options').textContent!)).toEqual({
            prompt_cache: { mode: 'anthropic_explicit', cache_history: false }, thinking: { type: 'adaptive', display: 'omitted' },
            context_management: { edits: [{ type: 'clear_thinking_20251015', keep: { type: 'thinking_turns', value: 1 } }] },
        });
        fireEvent.change(screen.getByRole('spinbutton', { name: 'Number of turns' }), { target: { value: '3' } });
        expect(JSON.parse(screen.getByTestId('options').textContent!).context_management.edits[0].keep.value).toBe(3);
        fireEvent.click(screen.getByRole('switch', { name: 'Enable provider thinking clearing' }));
        expect(JSON.parse(screen.getByTestId('options').textContent!)).toEqual(JSON.parse(initial));
    });
    it('does not replace malformed advanced JSON with defaults', () => {
        render(<Editor initial="{" />);
        expect(screen.getByText('Fix the invalid request-options JSON first.')).toBeInTheDocument();
        expect(screen.queryByRole('switch')).toBeNull();
        expect(screen.getByTestId('options')).toHaveTextContent('{');
    });
    it('rejects malformed objects and preserves the existing effort when changing clearing JSON', () => {
        expect(() => updateThinkingOption('[]', 'thinking', undefined)).toThrow();
        const result = updateThinkingOption('{"reasoning_effort":"max","thinking":{"type":"enabled"}}', 'thinking', { type: 'disabled' });
        expect(JSON.parse(result)).toEqual({ reasoning_effort: 'max', thinking: { type: 'disabled' } });
    });
});
