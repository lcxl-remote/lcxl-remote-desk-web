import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { AssistantCodeBlock } from './assistant-code-block';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
afterEach(() => vi.unstubAllGlobals());

describe('tool payload code block', () => {
    it('formats and highlights JSON without changing numbers, keys or escapes', async () => {
        const text = '{"id":9007199254740993,"id":1e+30,"path":"a\\nb","empty":[],"ok":true}';
        const writeText = vi.fn().mockResolvedValue(undefined);
        vi.stubGlobal('navigator', { clipboard: { writeText } });
        const { container } = render(<AssistantCodeBlock text={text} label="Input" />);
        const pre = container.querySelector('pre')!;
        expect(pre.textContent).toContain('"id": 9007199254740993');
        expect(pre.textContent).toContain('"id": 1e+30');
        expect(pre.textContent).toContain('"path": "a\\nb"');
        expect(pre.textContent).toContain('"empty": []');
        expect(pre.querySelectorAll('span').length).toBeGreaterThan(0);
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.codeBlock.copy' }));
        await waitFor(() => expect(writeText).toHaveBeenCalledWith(text));
        expect(pre).toHaveClass('whitespace-pre');
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.codeBlock.wrap' }));
        expect(pre).toHaveClass('whitespace-pre-wrap');
    });

    it('keeps plain output and HTML-like strings inert and reports clipboard failure', async () => {
        const text = '<script>alert(1)</script>\n  raw output';
        vi.stubGlobal('navigator', { clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) } });
        const { container, rerender } = render(<AssistantCodeBlock text={text} />);
        expect(container.querySelector('pre')?.textContent).toBe(text);
        expect(container.querySelector('script')).toBeNull();
        fireEvent.click(screen.getByRole('button', { name: 'pages.aiAssistant.codeBlock.copy' }));
        await screen.findByText('pages.aiAssistant.codeBlock.copyFailed');
        rerender(<AssistantCodeBlock text={'{"a":1}'} format="text" />);
        expect(container.querySelector('pre')?.textContent).toBe('{"a":1}');
    });

    it('retains large results without creating a span for every JSON token', () => {
        const text = JSON.stringify({ values: Array.from({ length: 20000 }, (_, i) => i) });
        const { container } = render(<AssistantCodeBlock text={text} />);
        const pre = container.querySelector('pre')!;
        expect(JSON.parse(pre.textContent!)).toEqual(JSON.parse(text));
        expect(pre.querySelector('span')).toBeNull();
        expect(pre).toHaveClass('max-h-80', 'overflow-auto', 'assistant-scrollbar');
    });
});
