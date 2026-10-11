import { describe, expect, it } from 'vitest';
import { MobileInteraction, type MouseIntent } from './mobile-interaction';

function setup(control = true) {
    const events: MouseIntent[] = [];
    const model = new MobileInteraction(event => events.push(event));
    model.updateGeometry(320, 240, 3840, 2160);
    model.setControl(control);
    return { model, events };
}
const p = (id: number, x: number, y: number) => ({ id, x, y });
function tap(m: MobileInteraction, x: number, y: number, time: number) {
    m.begin([p(1, x, y)], time);
    m.end([], [p(1, x, y)], time + 10);
}

describe('mobile remote interaction', () => {
    it('sends one click per tap and marks only the second of a pair as double', () => {
        const { model: m, events } = setup();
        tap(m, 160, 120, 0); tap(m, 160, 120, 100); tap(m, 160, 120, 200);
        expect(events.filter(e => e.event === 'mousedown')).toHaveLength(3);
        expect(events.filter(e => e.event === 'mouseup')).toHaveLength(3);
        expect(m.markers.map(e => e.kind)).toEqual(['single', 'double', 'single']);
        m.advanceTime(470);
        expect(m.markers.map(e => e.kind)).toEqual(['double', 'single']);
    });
    it('uses one long press right click, with no click on lift', () => {
        const { model: m, events } = setup();
        m.begin([p(7, 80, 120)], 0); m.advanceTime(500); m.advanceTime(1000);
        m.end([], [p(7, 80, 120)], 1100);
        expect(events.filter(e => e.event === 'mousedown').map(e => e.button)).toEqual([2]);
    });
    it('starts direct dragging at the initial target and releases at the final point', () => {
        const { model: m, events } = setup();
        m.begin([p(1, 80, 120)], 0); m.move([p(1, 160, 120)], 100);
        m.end([], [p(1, 240, 120)], 200);
        expect(events.find(e => e.event === 'mousedown')?.target.x).toBeCloseTo(.25);
        expect(events.at(-1)).toMatchObject({ event: 'mouseup', target: { x: .75, y: .5 } });
    });
    it('moves a local touchpad cursor relatively and clicks there instead of at the tap', () => {
        const { model: m, events } = setup(); m.setMode('touchpad');
        m.begin([p(1, 10, 10)], 0); m.move([p(1, 15, 10)], 50);
        expect(events).toHaveLength(0);
        m.move([p(1, 42, 10)], 100); m.move([p(1, 43, 10)], 120); m.end([], [], 150);
        expect(m.cursor?.x).toBeCloseTo(.5 + 33 / 320);
        tap(m, 10, 10, 200);
        expect(events.find(e => e.event === 'mousedown')?.target).toEqual(m.cursor);
    });
    it('cancels drag for two fingers, then consumes remaining touches without clicking', () => {
        const { model: m, events } = setup();
        m.begin([p(1, 80, 120)], 0); m.move([p(1, 120, 120)], 100);
        m.begin([p(1, 120, 120), p(2, 200, 120)], 150);
        m.move([p(1, 80, 120), p(2, 240, 120)], 200);
        m.end([p(2, 240, 120)], [p(1, 80, 120)], 250);
        m.end([], [p(2, 240, 120)], 300);
        expect(events.filter(e => e.event === 'mouseup')).toHaveLength(1);
        expect(events.filter(e => e.event === 'mousedown')).toHaveLength(1);
        expect(m.zoom).toBe(2);
    });
    it('supports 4K local zoom while viewing, with inverse projection and bounded pan', () => {
        const { model: m, events } = setup(false);
        expect(m.maxZoom).toBe(12);
        m.zoomBy(12);
        const target = { x: .5, y: .5 };
        expect(m.map(m.project(target))).toEqual(target);
        tap(m, 160, 120, 0);
        expect(events).toHaveLength(0);
        m.updateGeometry(240, 320, 3840, 2160);
        expect(m.maxZoom).toBe(16);
        expect(m.map({ x: 120, y: 160 })?.x).toBeCloseTo(.5);
    });
    it('clears held gestures and feedback on input loss and ignores black-bar clicks', () => {
        const { model: m, events } = setup();
        tap(m, 160, 1, 0); expect(events).toHaveLength(0);
        m.begin([p(1, 80, 120)], 100); m.move([p(1, 160, 120)], 200);
        m.setControl(false); m.advanceTime(1000);
        expect(events.at(-1)?.event).toBe('mouseup');
        expect(m.markers).toHaveLength(0);
        expect(m.cursor).toBeNull();
    });
    it('does not restart a gesture for repeated frame geometry notifications', () => {
        const { model: m, events } = setup();
        m.begin([p(1, 80, 120)], 0);
        m.updateGeometry(320, 240, 3840, 2160);
        m.end([], [p(1, 80, 120)], 100);
        expect(events.filter(e => e.event === 'mousedown')).toHaveLength(1);
    });
    it('does not right-click or left-click while a touchpad drag is only armed', () => {
        const { model: m, events } = setup(); m.setMode('touchpad'); m.toggleDrag();
        m.begin([p(1, 5, 5)], 0); m.advanceTime(1000); m.end([], [p(1, 5, 5)], 1100);
        expect(events).toHaveLength(0); expect(m.dragArmed).toBe(false);
        m.toggleDrag(); m.begin([p(2, 5, 5)], 1200); m.move([p(2, 45, 5)], 1300);
        m.setMode('direct'); m.cancel();
        expect(events.filter(e => e.event === 'mousedown')).toHaveLength(1);
        expect(events.filter(e => e.event === 'mouseup')).toHaveLength(1);
    });
    it('keeps unseen cursor targets stable and locates them without injecting input', () => {
        const { model: m, events } = setup(); m.setMode('touchpad');
        m.updatePhysicalPointer({ x: .9, y: .9 }); m.zoomBy(12);
        expect(m.cursorVisible).toBe(false); tap(m, 160, 120, 0);
        expect(events).toHaveLength(0);
        m.locateCursor(); expect(m.cursorVisible).toBe(true); expect(m.cursor).toEqual({ x: .9, y: .9 });
        expect(events).toHaveLength(0);
    });
    it('locks pinch priority and consumes a third finger until every finger lifts', () => {
        const { model: m, events } = setup();
        m.begin([p(1, 100, 120), p(2, 200, 120)], 0);
        m.move([p(1, 90, 140), p(2, 230, 140)], 100);
        expect(m.zoom).toBeCloseTo(1.4);
        expect(events).toHaveLength(0);
        m.begin([p(1, 90, 140), p(2, 230, 140), p(3, 160, 200)], 150);
        m.end([p(1, 90, 140)], [p(2, 230, 140), p(3, 160, 200)], 200);
        m.end([], [p(1, 90, 140)], 220);
        expect(events).toHaveLength(0);
    });
    it('preserves the viewing center through a temporary invalid frame size', () => {
        const { model: m } = setup(); m.setMode('touchpad'); m.zoomBy(12);
        m.updatePhysicalPointer({ x: .3, y: .4 }); m.locateCursor();
        m.updateGeometry(0, 0, 0, 0);
        expect(m.valid).toBe(false); expect(m.zoom).toBe(12);
        m.updateGeometry(240, 320, 3840, 2160);
        expect(m.map({ x: 120, y: 160 })?.x).toBeCloseTo(.3);
        expect(m.map({ x: 120, y: 160 })?.y).toBeCloseTo(.4);
    });

});
