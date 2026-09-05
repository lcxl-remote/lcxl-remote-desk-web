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
        expect(zh['pages.deskDashboard.deviceAssistant']).toBe('AI助手');
        expect(zh['pages.deviceAssistant.title']).toBe('AI助手');
        expect(en['pages.deskDashboard.deviceAssistant']).toBe('AI Assistant');
        expect(en['pages.deviceAssistant.title']).toBe('AI Assistant');
        for (const locale of [zh, en]) {
            expect(Object.values(locale).join('\n')).not.toMatch(/设备助手|Device Assistant/);
        }
    });
});
