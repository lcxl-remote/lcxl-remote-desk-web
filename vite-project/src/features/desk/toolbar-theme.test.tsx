import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { render, screen } from '@testing-library/react';
import { expect, it } from 'vitest';
import { Maximize } from 'lucide-react';
import { Button } from '@/components/ui/button';

function color(value: string) {
    return value.match(/[\d.]+/g)!.map(Number);
}
function blend(foreground: number[], background: number[]) {
    const alpha = foreground[3] ?? 1;
    return foreground.slice(0, 3).map((channel, i) => channel * alpha + background[i] * (1 - alpha));
}
function luminance(rgb: number[]) {
    const linear = rgb.map(channel => {
        const value = channel / 255;
        return value <= .04045 ? value / 12.92 : ((value + .055) / 1.055) ** 2.4;
    });
    return linear[0] * .2126 + linear[1] * .7152 + linear[2] * .0722;
}
it.each(['light', 'dark'])('keeps toolbar icons legible in every %s theme interaction state', theme => {
    const tokens = readFileSync(resolve(process.cwd(), 'src/index.css'), 'utf8');
    const root = tokens.match(/:root\s*\{([^}]+)\}/)![1];
    const dark = tokens.match(/\.dark\s*\{([^}]+)\}/)![1];
    const variables = Object.fromEntries([...`${root}${theme === 'dark' ? dark : ''}`.matchAll(/(--[\w-]+):\s*([^;]+);/g)]
        .map(match => [match[1], match[2]]));
    const sheet = document.createElement('style');
    // Resolve production tokens and expose pseudo states without relying on jsdom pointer styling.
    sheet.textContent = readFileSync(resolve(process.cwd(), 'src/features/desk/desk-session.css'), 'utf8')
        .replace(/var\((--[\w-]+)\)/g, (match, key) => variables[key] ?? match)
        .replace(/\.controlButton:hover/g, '.controlButton[data-hovered]')
        .replace(/\.controlButton:active/g, '.controlButton[data-pressed]')
        .replace(/\.controlBarDragHandle:hover/g, '.controlBarDragHandle[data-hovered]');
    document.head.append(sheet);
    try {
        const { container } = render(<div className="controlBar">
            <Button className="controlButton" variant="ghost" aria-label="Action"><Maximize /></Button>
            <button className="controlBarDragHandle" aria-label="Handle"><Maximize /></button>
        </div>);
        const background = blend(color(getComputedStyle(container.querySelector('.controlBar')!).backgroundColor), [0, 0, 0]);
        for (const name of ['Action', 'Handle']) {
            const button = screen.getByRole('button', { name });
            for (const state of ['normal', 'hovered', 'pressed']) {
                button.toggleAttribute('data-hovered', state !== 'normal');
                button.toggleAttribute('data-pressed', state === 'pressed');
                const style = getComputedStyle(button);
                const foreground = luminance(blend(color(style.color), background));
                const surface = luminance(blend(color(style.backgroundColor), background));
                const contrast = (Math.max(foreground, surface) + .05) / (Math.min(foreground, surface) + .05);
                expect(contrast, `${theme} ${name} ${state}`).toBeGreaterThanOrEqual(4.5);
                expect(getComputedStyle(button.querySelector('svg')!).color).toBe(style.color);
            }
        }
    } finally { sheet.remove(); }
});
