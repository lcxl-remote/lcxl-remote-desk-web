import { computeVideoContentRect, type VideoContentRect } from './video-content-rect';

export type Point = { x: number; y: number };
export type TouchPoint = Point & { id: number };
export type TouchMode = 'direct' | 'touchpad';
export type MouseIntent = {
    event: 'position' | 'mousemove' | 'mousedown' | 'mouseup' | 'wheel';
    target: Point;
    button: number;
    buttons: number;
    deltaX: number;
    deltaY: number;
};
export type ClickMarker = { id: number; target: Point; kind: 'single' | 'double' | 'right' | 'down'; expires: number };
type SingleGesture = {
    kind: 'single'; id: number; start: Point; last: Point; target: Point | null;
    started: number; role: 'pending' | 'moving' | 'dragging' | 'consumed'; armed: boolean;
};
type MultiGesture = {
    kind: 'multi'; ids: [number, number]; start: Point; previous: Point; span: number;
    zoom: number; pan: Point; anchor: Point;
    role: 'pending' | 'pinch' | 'pan' | 'scroll';
};
type Gesture = SingleGesture | MultiGesture | { kind: 'idle' | 'consumed' };

const clamp = (n: number, low: number, high: number) => Math.max(low, Math.min(n, high));
const distance = (a: Point, b: Point) => Math.hypot(a.x - b.x, a.y - b.y);
const center = (a: Point, b: Point): Point => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });

/** Pure local gesture and viewport state. Input transport remains in the session adapter. */
export class MobileInteraction {
    mode: TouchMode = 'direct';
    panMode: 'pan' | 'scroll' = 'pan';
    zoom = 1;
    maxZoom = 3;
    pan: Point = { x: 0, y: 0 };
    cursor: Point | null = null;
    dragArmed = false;
    canControl = false;
    valid = false;
    width = 0;
    height = 0;
    content: VideoContentRect | null = null;
    markers: ClickMarker[] = [];
    private gesture: Gesture = { kind: 'idle' };
    private touchCount = 0;
    private markerId = 0;
    private lastTap: { target: Point; time: number } | null = null;
    private savedCenter: Point | null = null;
    private frameSize: Point = { x: 0, y: 0 };
    private readonly send: (intent: MouseIntent) => void;
    readonly slop: number;
    readonly longPressMs: number;

    constructor(
        send: (intent: MouseIntent) => void,
        slop = 8,
        longPressMs = 500,
    ) {
        this.send = send;
        this.slop = slop;
        this.longPressMs = longPressMs;
    }

    updateGeometry(width: number, height: number, frameWidth: number, frameHeight: number) {
        const values = [width, height, frameWidth, frameHeight];
        const content = values.every(value => Number.isFinite(value) && value > 0)
            ? computeVideoContentRect(width, height, frameWidth, frameHeight) : null;
        if (!content) {
            if (this.valid) this.savedCenter = this.map({ x: this.width / 2, y: this.height / 2 }, true);
            this.cancel(true);
            this.valid = false;
            return;
        }
        if (this.valid && width === this.width && height === this.height
            && frameWidth === this.frameSize.x && frameHeight === this.frameSize.y) return;
        const oldCenter = this.valid ? this.map({ x: this.width / 2, y: this.height / 2 }, true) : this.savedCenter;
        this.savedCenter = null;
        this.cancel(true);
        this.width = width;
        this.height = height;
        this.content = content;
        this.frameSize = { x: frameWidth, y: frameHeight };
        this.valid = true;
        const fitScale = Math.min(content.width / frameWidth, content.height / frameHeight);
        this.maxZoom = Math.min(16, Math.max(3, 1 / fitScale));
        this.zoom = clamp(this.zoom, 1, this.maxZoom);
        if (oldCenter) this.pan = this.panFor(oldCenter, { x: width / 2, y: height / 2 });
        this.clampPan();
    }

    setControl(available: boolean) {
        if (this.canControl && !available) {
            this.cancel();
            this.markers = [];
            this.cursor = null;
            this.dragArmed = false;
        }
        this.canControl = available;
        if (available && this.mode === 'touchpad' && !this.cursor && this.valid) {
            this.cursor = this.map({ x: this.width / 2, y: this.height / 2 }, true);
        }
    }

    setMode(mode: TouchMode) {
        this.cancel(true);
        this.mode = mode;
        this.dragArmed = false;
        this.markers = [];
        if (mode === 'touchpad' && !this.cursor && this.valid) {
            this.cursor = this.map({ x: this.width / 2, y: this.height / 2 }, true);
        }
    }

    toggleDrag() {
        const old = this.dragArmed;
        this.cancel(true);
        this.dragArmed = this.mode === 'touchpad' && this.canControl && this.cursorVisible && !old;
    }

    setPanMode(mode: 'pan' | 'scroll') {
        if (this.touchCount) return;
        this.panMode = mode;
        this.lastTap = null;
    }

    resetViewport() {
        this.cancel(true);
        this.zoom = 1;
        this.pan = { x: 0, y: 0 };
        this.panMode = 'pan';
        this.clampPan();
    }

    resetTarget() {
        this.resetViewport();
        this.cursor = null;
        this.markers = [];
        this.dragArmed = false;
        this.valid = false;
        this.content = null;
        this.savedCenter = null;
    }

    zoomBy(factor: number) {
        if (!this.valid) return;
        this.cancel(true);
        const focal = { x: this.width / 2, y: this.height / 2 };
        const anchor = this.map(focal, true);
        if (!anchor) return;
        this.zoom = clamp(this.zoom * factor, 1, this.maxZoom);
        this.pan = this.panFor(anchor, focal);
        this.clampPan();
        if (this.zoom === 1) this.panMode = 'pan';
        this.hideOffscreenDrag();
    }

    project(target: Point): Point {
        const content = this.content;
        if (!content) return { x: 0, y: 0 };
        return {
            x: content.offsetX + target.x * content.width * this.zoom + this.pan.x,
            y: content.offsetY + target.y * content.height * this.zoom + this.pan.y,
        };
    }

    map(point: Point, clampOutside = false): Point | null {
        const content = this.content;
        if (!this.valid || !content) return null;
        const x = (point.x - content.offsetX - this.pan.x) / (content.width * this.zoom);
        const y = (point.y - content.offsetY - this.pan.y) / (content.height * this.zoom);
        if (!clampOutside && (x < 0 || x > 1 || y < 0 || y > 1)) return null;
        return { x: clamp(x, 0, 1), y: clamp(y, 0, 1) };
    }

    get cursorVisible(): boolean {
        if (!this.cursor || !this.valid) return false;
        const p = this.project(this.cursor);
        return p.x >= 0 && p.x <= this.width && p.y >= 0 && p.y <= this.height;
    }

    locateCursor() {
        if (!this.cursor || !this.valid) return;
        this.cancel(true);
        this.pan = this.panFor(this.cursor, { x: this.width / 2, y: this.height / 2 });
        this.clampPan();
    }

    updatePhysicalPointer(target: Point) {
        this.cursor = target;
    }

    get videoTransform(): string {
        const content = this.content;
        const x = this.pan.x + (content?.offsetX ?? 0) * (1 - this.zoom);
        const y = this.pan.y + (content?.offsetY ?? 0) * (1 - this.zoom);
        return `translate(${x}px, ${y}px) scale(${this.zoom})`;
    }

    get nextDeadline(): number | null {
        let deadline: number | null = null;
        if (this.gesture.kind === 'single' && this.gesture.role === 'pending'
            && !this.gesture.armed && this.canControl) {
            deadline = this.gesture.started + this.longPressMs;
        }
        for (const marker of this.markers) {
            deadline = deadline === null ? marker.expires : Math.min(deadline, marker.expires);
        }
        return deadline;
    }

    advanceTime(now: number) {
        this.markers = this.markers.filter(marker => marker.expires > now);
        const g = this.gesture;
        if (g.kind === 'single' && g.role === 'pending' && !g.armed
            && now >= g.started + this.longPressMs && this.canControl) {
            if (g.target && (this.mode === 'direct' || this.cursorVisible)) {
                this.click(g.target, 2, 'right', now);
            }
            this.lastTap = null;
            g.role = 'consumed';
        }
    }

    begin(points: TouchPoint[], now: number) {
        this.touchCount = points.length;
        if (!this.valid || !points.length) return;
        if (points.length > 2) {
            this.cancel(true);
            return;
        }
        if (this.gesture.kind === 'consumed') return;
        if (points.length === 2) {
            this.finishDrag();
            this.dragArmed = false;
            this.lastTap = null;
            const [a, b] = points;
            const c = center(a, b);
            const anchor = this.map(c, true);
            if (!anchor) return;
            this.gesture = {
                kind: 'multi', ids: [a.id, b.id], start: c, previous: c,
                span: Math.max(distance(a, b), 1), zoom: this.zoom,
                pan: { ...this.pan }, anchor, role: 'pending',
            };
            return;
        }
        if (this.gesture.kind !== 'idle') return;
        const point = points[0];
        if (this.mode === 'touchpad' && !this.cursor) {
            this.cursor = this.map({ x: this.width / 2, y: this.height / 2 }, true);
        }
        const target = this.mode === 'touchpad' ? this.cursor : this.map(point);
        this.gesture = {
            kind: 'single', id: point.id, start: point, last: point, target,
            started: now, role: this.canControl && target ? 'pending' : 'consumed', armed: this.dragArmed,
        };
    }

    move(points: TouchPoint[], now: number) {
        this.touchCount = points.length;
        if (points.length > 2) { this.cancel(true); return; }
        const g = this.gesture;
        if (g.kind === 'single') {
            if (points.length !== 1) { this.begin(points, now); return; }
            const point = points.find(p => p.id === g.id);
            if (!point || g.role === 'consumed' || !this.canControl || !this.content) return;
            if (g.role === 'pending') {
                if (distance(point, g.start) <= this.slop) return;
                this.lastTap = null;
                if (this.mode === 'direct' || g.armed) {
                    if (!g.target || (this.mode === 'touchpad' && !this.cursorVisible)) {
                        g.role = 'consumed'; this.dragArmed = false; return;
                    }
                    this.emit('position', g.target);
                    this.emit('mousedown', g.target, 0, 1);
                    this.mark(g.target, 'down', now);
                    g.role = 'dragging';
                } else {
                    g.role = 'moving';
                    if (!this.cursorVisible) this.locateWithoutCancel();
                }
            }
            if (this.mode === 'touchpad') {
                const cursor = this.cursor ?? g.target;
                if (!cursor) return;
                this.cursor = {
                    x: clamp(cursor.x + (point.x - g.last.x) / (this.content.width * this.zoom), 0, 1),
                    y: clamp(cursor.y + (point.y - g.last.y) / (this.content.height * this.zoom), 0, 1),
                };
                this.followCursor();
            } else {
                this.cursor = this.map(point, true);
            }
            if (this.cursor) this.emit('mousemove', this.cursor, 0, g.role === 'dragging' ? 1 : 0);
            g.last = point;
            return;
        }
        if (g.kind !== 'multi') return;
        const a = points.find(p => p.id === g.ids[0]);
        const b = points.find(p => p.id === g.ids[1]);
        if (!a || !b) { this.gesture = { kind: points.length ? 'consumed' : 'idle' }; return; }
        const c = center(a, b);
        const span = Math.max(distance(a, b), 1);
        if (g.role === 'pending') {
            if (Math.abs(span - g.span) > this.slop) g.role = 'pinch';
            else if (distance(c, g.start) > this.slop) {
                g.role = !this.canControl || (this.zoom > 1 && this.panMode === 'pan') ? 'pan' : 'scroll';
            } else return;
        }
        if (g.role === 'pinch') {
            this.zoom = clamp(g.zoom * span / g.span, 1, this.maxZoom);
            this.pan = this.panFor(g.anchor, c);
            this.clampPan();
            if (this.zoom === 1) this.panMode = 'pan';
        } else if (g.role === 'pan') {
            this.pan = { x: g.pan.x + c.x - g.start.x, y: g.pan.y + c.y - g.start.y };
            this.clampPan();
        } else if (this.canControl) {
            const target = this.mode === 'touchpad' ? this.cursor : this.map(c, true);
            if (target && (this.mode === 'direct' || this.cursorVisible)) {
                this.emit('position', target);
                this.emit('wheel', target, 0, 0, -(c.x - g.previous.x), -(c.y - g.previous.y));
            }
        }
        g.previous = c;
        this.hideOffscreenDrag();
    }

    end(points: TouchPoint[], changed: TouchPoint[], now: number) {
        const g = this.gesture;
        if (g.kind === 'single' && !points.length) {
            const final = changed.find(p => p.id === g.id);
            if (final && g.role === 'dragging') this.move([final], now);
            if (g.role === 'dragging') this.finishDrag();
            else if (g.role === 'pending' && this.canControl && g.target) {
                if (g.armed) this.dragArmed = false;
                else if (this.mode === 'direct' || this.cursorVisible) {
                    const previous = this.lastTap;
                    const double = previous && now - previous.time <= 350
                        && distance(this.project(previous.target), this.project(g.target)) <= 24;
                    this.click(g.target, 0, double ? 'double' : 'single', now);
                    this.lastTap = double ? null : { target: g.target, time: now };
                }
            }
        }
        this.touchCount = points.length;
        this.gesture = { kind: points.length ? 'consumed' : 'idle' };
    }

    cancel(consume = false) {
        this.finishDrag();
        this.lastTap = null;
        this.gesture = { kind: consume && this.touchCount ? 'consumed' : 'idle' };
    }

    private emit(event: MouseIntent['event'], target: Point, button = 0, buttons = 0, deltaX = 0, deltaY = 0) {
        if (this.canControl && this.valid) this.send({ event, target, button, buttons, deltaX, deltaY });
    }

    private click(target: Point, button: number, kind: ClickMarker['kind'], now: number) {
        this.cursor = target;
        this.emit('position', target);
        this.emit('mousedown', target, button, button === 2 ? 2 : 1);
        this.emit('mouseup', target, button);
        this.mark(target, kind, now);
    }

    private mark(target: Point, kind: ClickMarker['kind'], now: number) {
        this.markers = [...this.markers.filter(marker => marker.expires > now), {
            id: ++this.markerId, target: { ...target }, kind, expires: now + 450,
        }].slice(-4);
    }

    private finishDrag() {
        if (this.gesture.kind === 'single' && this.gesture.role === 'dragging') {
            const target = this.cursor ?? this.gesture.target;
            if (target) { this.emit('position', target); this.emit('mouseup', target); }
            this.gesture.role = 'consumed';
            this.dragArmed = false;
        }
    }

    private panFor(anchor: Point, point: Point): Point {
        const c = this.content;
        return c ? {
            x: point.x - c.offsetX - anchor.x * c.width * this.zoom,
            y: point.y - c.offsetY - anchor.y * c.height * this.zoom,
        } : { x: 0, y: 0 };
    }

    private clampPan() {
        const c = this.content;
        if (!c) return;
        const w = c.width * this.zoom;
        const h = c.height * this.zoom;
        this.pan = {
            x: w >= this.width ? clamp(this.pan.x, this.width - c.offsetX - w, -c.offsetX)
                : (this.width - w) / 2 - c.offsetX,
            y: h >= this.height ? clamp(this.pan.y, this.height - c.offsetY - h, -c.offsetY)
                : (this.height - h) / 2 - c.offsetY,
        };
    }

    private locateWithoutCancel() {
        if (!this.cursor) return;
        this.pan = this.panFor(this.cursor, { x: this.width / 2, y: this.height / 2 });
        this.clampPan();
    }

    private followCursor() {
        if (!this.cursor) return;
        const p = this.project(this.cursor);
        const margin = Math.min(24, this.width / 4, this.height / 4);
        this.pan.x += clamp(p.x, margin, this.width - margin) - p.x;
        this.pan.y += clamp(p.y, margin, this.height - margin) - p.y;
        this.clampPan();
    }

    private hideOffscreenDrag() {
        if (!this.cursorVisible) this.dragArmed = false;
    }
}
