import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { AiAssistantIcon } from './ai-assistant-icon';
import zh from '@/locales/zh-CN/pages';
import en from '@/locales/en-US/pages';

describe('AI Assistant branding', () => {
    it('renders two scalable decorative sparkles with inherited color', () => {
        const { container } = render(<AiAssistantIcon className="h-5 w-5 text-violet-500" />);
        const svg = container.querySelector('svg')!;
        expect(svg.getAttribute('viewBox')).toBe('0 0 24 24');
        expect(svg.getAttribute('fill')).toBe('currentColor');
        expect(svg.getAttribute('aria-hidden')).toBe('true');
        expect(svg.getAttribute('class')).toContain('h-5 w-5');
        expect(svg.querySelectorAll('path')).toHaveLength(2);
    });

    it('uses the same product name on the dashboard and assistant page in both locales', () => {
        expect(zh['pages.deskDashboard.aiAssistant']).toBe('AI助手');
        expect(zh['pages.aiAssistant.title']).toBe('AI助手');
        expect(en['pages.deskDashboard.aiAssistant']).toBe('AI Assistant');
        expect(en['pages.aiAssistant.title']).toBe('AI Assistant');
        for (const locale of [zh, en]) {
            for (const [key, value] of Object.entries(locale)) expect(value, key).not.toMatch(new RegExp('设备\\s*(?:AI\\s*)?助手|Device\\s+Assistant|co' + 'pilot', 'i'));
        }
    });
});
