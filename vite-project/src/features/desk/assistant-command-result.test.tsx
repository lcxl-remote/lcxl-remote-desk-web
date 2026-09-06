import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantCommandResult, parseCommandReceipt } from './assistant-command-result';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ i18n: { language: 'en' }, t: (key: string, options?: { value: string }) => options ? `${key}: ${options.value}` : key }) }));

const receipt = {
    exit_code: 1, duration_ms: 1234,
    streams: { type: 'split', stdout: '12G\t/example\n', stderr: 'Permission denied', stdout_truncated: true, stderr_truncated: false },
    redactions: ['secret removed'],
};
const wire = (value: unknown) => JSON.stringify({ Exec: value });

describe('command receipt presentation', () => {
    it('parses the actual tagged device receipt without altering its output', () => {
        expect(parseCommandReceipt(wire(receipt))).toEqual(receipt);
    });

    it.each(['execution failed: timeout', '{bad json', 'null', '[]', '{}', '{"Exec":null}', wire({ ...receipt, duration_ms: -1 }), wire({ ...receipt, exit_code: '0' }), wire({ ...receipt, streams: { type: 'unknown' } }), wire({ ...receipt, redactions: [123] })])('preserves unrecognized content: %s', text => {
        expect(parseCommandReceipt(text)).toBeNull();
        const { container } = render(<AssistantCommandResult text={text} />);
        expect(container.querySelector('details')?.open).toBe(false);
        expect(container.querySelector('pre')?.textContent).toBe(text);
    });

    it('is collapsed initially and expands into labeled fields with independently collapsed raw data', () => {
        const text = wire(receipt);
        const { container, rerender } = render(<AssistantCommandResult text={text} />);
        const details = container.querySelector('details')!;
        expect(details.open).toBe(false);
        fireEvent.click(details.querySelector('summary')!);
        expect(details.open).toBe(true);
        expect(screen.getByText('pages.deviceAssistant.commandReceipt.exitCode')).toBeTruthy();
        expect(screen.getByText('1')).toBeTruthy();
        expect(screen.getByText('pages.deviceAssistant.commandReceipt.milliseconds: 1,234')).toBeTruthy();
        expect(screen.getByText('Permission denied')).toBeTruthy();
        expect(screen.getByText('pages.deviceAssistant.commandReceipt.truncated')).toBeTruthy();
        expect(screen.getByText('secret removed')).toBeTruthy();
        expect(details.querySelector('details')?.open).toBe(false);
        rerender(<AssistantCommandResult text={text} />);
        expect(details.open).toBe(true);
        fireEvent.click(details.querySelector('summary')!);
        expect(details.open).toBe(false);
    });

    it('keeps PTY output combined and renders device text without interpreting HTML or Markdown', () => {
        const terminal = '<img src=x onerror=alert(1)>\n[link](https://example.com)';
        const { container } = render(<AssistantCommandResult text={wire({ exit_code: 0, duration_ms: 0, streams: { type: 'pty_combined', terminal, truncated: true } })} />);
        expect(screen.getByText('pages.deviceAssistant.commandReceipt.terminal')).toBeTruthy();
        expect(screen.queryByText('pages.deviceAssistant.commandReceipt.stderr')).toBeNull();
        expect(container.querySelector('pre')?.textContent).toBe(terminal);
        expect(container.querySelector('img, a')).toBeNull();
    });

    it('shows empty streams explicitly and no truncation warning when complete', () => {
        render(<AssistantCommandResult text={wire({ ...receipt, redactions: [], streams: { type: 'split', stdout: '', stderr: '', stdout_truncated: false, stderr_truncated: false } })} />);
        expect(screen.getAllByText('pages.deviceAssistant.commandReceipt.empty')).toHaveLength(2);
        expect(screen.queryByText('pages.deviceAssistant.commandReceipt.truncated')).toBeNull();
        expect(screen.queryByText('pages.deviceAssistant.commandReceipt.redactions')).toBeNull();
    });
});
