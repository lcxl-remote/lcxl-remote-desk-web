import { describe, expect, it } from 'vitest';
import { DEFAULT_TOOLBAR_PREFERENCES, TOOLBAR_EDGES, TOOLBAR_STORAGE_KEY, ToolbarDrag, floatingPlacement, readToolbarPreferences, toolbarCandidate, toolbarLayout, writeToolbarPreferences } from './toolbar-layout';
import type { ToolbarPlacement } from './toolbar-layout';
const bounds = { x: 10, y: 20, width: 1000, height: 700 };
const size = { width: 300, height: 56 };
const floating: ToolbarPlacement = { kind: 'floating', x: .5, y: .5 };
describe('toolbar layout and placement', () => {
    it.each(TOOLBAR_EDGES)('reserves the %s rail without intersecting the video', edge => {
        const vertical = edge === 'left' || edge === 'right';
        const layout = toolbarLayout(bounds, { kind: 'docked', edge, along: .5 }, vertical ? { width: 56, height: 300 } : size, 56);
        expect(layout.video.width).toBe(vertical ? 944 : 1000);
        expect(layout.video.height).toBe(vertical ? 700 : 644);
        const v = layout.video, b = layout.bar;
        expect(b.x + b.width <= v.x || v.x + v.width <= b.x || b.y + b.height <= v.y || v.y + v.height <= b.y).toBe(true);
    });
    it('round trips floating anchors and reprojects them for another window size', () => {
        const bar = toolbarLayout(bounds, floating, size, 56).bar;
        expect(floatingPlacement(bar, bounds)).toEqual(floating);
        const changed = toolbarLayout({ ...bounds, width: 500 }, floating, size, 56);
        expect(changed.video).toEqual({ ...bounds, width: 500 });
        expect(changed.bar.x).toBe(110);
    });
    it('does not turn zero space into negative dimensions or nonfinite positions', () => {
        const l = toolbarLayout({ x: 0, y: 0, width: 0, height: 0 }, { kind: 'docked', edge: 'left', along: 1 }, size, 56);
        expect(l.video.width).toBe(0); expect(l.bar.width).toBe(0);
        expect(Object.values(l.bar).every(Number.isFinite)).toBe(true);
    });
    it('uses finite edges, stable corner ties and touch hysteresis', () => {
        expect(toolbarCandidate({ x: 10, y: 20 }, bounds, null, false, 56)).toBe('top');
        expect(toolbarCandidate({ x: 30, y: 40 }, bounds, 'left', false, 56)).toBe('left');
        expect(toolbarCandidate({ x: 10, y: -500 }, bounds, null, false, 56)).toBeNull();
        expect(toolbarCandidate({ x: 40, y: 350 }, bounds, null, false, 56)).toBeNull();
        expect(toolbarCandidate({ x: 40, y: 350 }, bounds, null, true, 56)).toBe('left');
    });
    it('keeps the original placement below slop and commits snapping only at finish', () => {
        const bar = toolbarLayout(bounds, floating, size, 56).bar;
        const drag = new ToolbarDrag(floating, bounds, bar, { x: bar.x + 20, y: bar.y + 20 }, false, 56);
        drag.move({ x: bar.x + 22, y: bar.y + 21 });
        expect(drag.finish(size)).toEqual(floating);
        drag.move({ x: 12, y: 200 });
        expect(drag.original).toEqual(floating); expect(drag.candidate).toBe('left');
        expect(drag.finish({ width: 56, height: 300 })).toMatchObject({ kind: 'docked', edge: 'left' });
    });
    it('slides along an attached edge through a corner, then detaches only inward', () => {
        const placement: ToolbarPlacement = { kind: 'docked', edge: 'left', along: .5 };
        const bar = toolbarLayout(bounds, placement, { width: 56, height: 300 }, 56).bar;
        const drag = new ToolbarDrag(placement, bounds, bar, { x: 30, y: bar.y + 20 }, true, 56);
        drag.move({ x: 30, y: 22 });
        expect(drag.candidate).toBe('left'); expect(drag.detached).toBe(false);
        drag.move({ x: 90, y: 300 });
        expect(drag.detached).toBe(true); expect(drag.candidate).toBeNull();
        expect(drag.finish(size).kind).toBe('floating');
    });
    it('retains the saved along anchor when a docked toolbar fills the edge', () => {
        const placement: ToolbarPlacement = { kind: 'docked', edge: 'left', along: .8 };
        const tallSize = { width: 56, height: bounds.height };
        const bar = toolbarLayout(bounds, placement, tallSize, 56).bar;
        const drag = new ToolbarDrag(placement, bounds, bar, { x: bar.x + 20, y: bar.y + 20 }, true, 56);
        drag.move({ x: bar.x + 20, y: bar.y + 80 });
        expect(drag.finish(tallSize)).toEqual(placement);
        const restored = toolbarLayout({ ...bounds, height: 1000 }, placement, tallSize, 56);
        expect(restored.bar.y).toBe(bounds.y + .8 * 300);
    });
});
describe('local toolbar preferences', () => {
    it('persists a complete user choice and restores it independently of connection identities', () => {
        const storage = new Map<string, string>();
        const adapter = { getItem: (k: string) => storage.get(k) ?? null, setItem: (k: string, v: string) => { storage.set(k, v); } };
        const preferences = { version: 1 as const, placement: { kind: 'docked' as const, edge: 'right' as const, along: .7 }, expanded: true };
        writeToolbarPreferences(adapter, preferences);
        expect(storage.size).toBe(1); expect(storage.has(TOOLBAR_STORAGE_KEY)).toBe(true);
        expect(readToolbarPreferences(adapter)).toEqual(preferences);
    });
    it.each(['bad', '{"version":2}', '{"version":1,"expanded":true,"placement":{"kind":"floating","x":3,"y":0}}'])('falls back for an invalid record %s', raw => {
        expect(readToolbarPreferences({ getItem: () => raw })).toEqual(DEFAULT_TOOLBAR_PREFERENCES);
    });
    it('keeps the local UI working when browser storage fails', () => {
        expect(readToolbarPreferences({ getItem: () => { throw Error('blocked'); } })).toEqual(DEFAULT_TOOLBAR_PREFERENCES);
        expect(() => writeToolbarPreferences({ setItem: () => { throw Error('quota'); } }, DEFAULT_TOOLBAR_PREFERENCES)).not.toThrow();
    });
});
