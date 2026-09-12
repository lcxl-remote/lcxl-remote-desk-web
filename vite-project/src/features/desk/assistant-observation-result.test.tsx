import { render, screen, fireEvent } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AssistantObservationResult } from './assistant-observation-result';
import zh from '@/locales/zh-CN/pages';

vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string, options?: Record<string, unknown>) => {
    let text = (zh as Record<string, string>)[key] ?? key;
    for (const [name, value] of Object.entries(options ?? {})) text = text.replace(`{{${name}}}`, String(value));
    return text;
} }) }));

describe('observation summaries', () => {
    it('shows desktop facts with native references confined to collapsed details', () => {
        const { container } = render(<AssistantObservationResult data={{ ReadContext: { DesktopSessionInspect: { os: 'macos', active_application_name: 'Calculator', session: { token: 'internal-token' } } } }} />);
        expect(screen.getByText('桌面可访问')).toBeTruthy();
        expect(screen.getByText('macOS')).toBeTruthy();
        expect(screen.getByText('Calculator')).toBeTruthy();
        const details = container.querySelector('details')!;
        expect(details.open).toBe(false);
        expect(details.textContent).toContain('internal-token');
        fireEvent.click(details.querySelector('summary')!);
        expect(details.open).toBe(true);
    });
    it('shows native control labels, values and incomplete results without exposing protected values in the summary', () => {
        const { container } = render(<AssistantObservationResult data={{ ReadContext: { DesktopUiInspect: { truncated: true, nodes: [
            { role: 'AXButton', name: '计算', enabled: true, supported_actions: ['invoke'] },
            { role: 'text_field', name: '结果', value: '42', enabled: false },
            { role: 'text_field', name: '密码', value: 'must-not-render', is_protected: true },
        ] } } }} />);
        expect(screen.getByText('按钮')).toBeTruthy();
        expect(screen.getByText('内容：42')).toBeTruthy();
        expect(screen.getByText('支持操作：激活')).toBeTruthy();
        expect(screen.getByText('不可操作')).toBeTruthy();
        expect(screen.getByText('结果达到读取上限，仅展示部分内容。')).toBeTruthy();
        expect(container.querySelector('ul')!.textContent).not.toContain('must-not-render');
    });
    it.each([null, {}, { ReadContext: { DesktopUiInspect: { nodes: [] } } }])('handles empty or unrecognized results', data => {
        const { container } = render(<AssistantObservationResult data={data} />);
        expect(container.querySelector('details')!.open).toBe(false);
        expect(container.querySelector('pre')!.textContent).toBe(JSON.stringify(data, null, 2));
    });
});
