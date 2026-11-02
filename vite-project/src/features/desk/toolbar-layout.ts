export type ToolbarEdge = 'top' | 'bottom' | 'left' | 'right';
export type ToolbarPlacement = { kind: 'floating'; x: number; y: number }
    | { kind: 'docked'; edge: ToolbarEdge; along: number };
export type ToolbarPreferences = { version: 1; placement: ToolbarPlacement; expanded: boolean };
export type ToolbarPoint = { x: number; y: number };
export type ToolbarRect = ToolbarPoint & { width: number; height: number };
export type ToolbarSize = { width: number; height: number };
export const TOOLBAR_STORAGE_KEY = 'lrdm.remoteDesk.toolbarLayout.v1';
export const TOOLBAR_EDGES: ToolbarEdge[] = ['top', 'bottom', 'left', 'right'];
export const DEFAULT_TOOLBAR_PREFERENCES: ToolbarPreferences = {
    version: 1, placement: { kind: 'floating', x: .5, y: 1 }, expanded: false,
};

const clamp = (n: number, max: number) => Math.max(0, Math.min(n, Math.max(0, max)));
export const isVerticalToolbar = (edge: ToolbarEdge | null) => edge === 'left' || edge === 'right';
export const toolbarEdge = (placement: ToolbarPlacement) => placement.kind === 'docked' ? placement.edge : null;
export function readToolbarPreferences(storage: Pick<Storage, 'getItem'> | null): ToolbarPreferences {
    try {
        const raw = storage?.getItem(TOOLBAR_STORAGE_KEY);
        const value = raw ? JSON.parse(raw) : null;
        const p = value?.placement;
        const ratio = (v: unknown) => typeof v === 'number' && Number.isFinite(v) && v >= 0 && v <= 1;
        if (value?.version === 1 && typeof value.expanded === 'boolean' && (
            p?.kind === 'floating' && ratio(p.x) && ratio(p.y)
            || p?.kind === 'docked' && TOOLBAR_EDGES.includes(p.edge) && ratio(p.along)
        )) return { version: 1, placement: p.kind === 'floating' ? { kind: 'floating', x: p.x, y: p.y }
            : { kind: 'docked', edge: p.edge, along: p.along }, expanded: value.expanded };
    } catch { /* Storage can be unavailable or contain an older malformed record. */ }
    return { ...DEFAULT_TOOLBAR_PREFERENCES, placement: { ...DEFAULT_TOOLBAR_PREFERENCES.placement } };
}
export function writeToolbarPreferences(storage: Pick<Storage, 'setItem'> | null, value: ToolbarPreferences) {
    try { storage?.setItem(TOOLBAR_STORAGE_KEY, JSON.stringify(value)); }
    catch { /* The current window retains its working layout. */ }
}

export function floatingBounds(bounds: ToolbarRect): ToolbarRect {
    const x = Math.min(16, Math.max(0, bounds.width / 4));
    const y = Math.min(16, Math.max(0, bounds.height / 4));
    return { x: bounds.x + x, y: bounds.y + y, width: Math.max(0, bounds.width - 2 * x), height: Math.max(0, bounds.height - 2 * y) };
}
export function constrainToolbar(rect: ToolbarRect, bounds: ToolbarRect): ToolbarRect {
    return { ...rect, x: bounds.x + clamp(rect.x - bounds.x, bounds.width - rect.width),
        y: bounds.y + clamp(rect.y - bounds.y, bounds.height - rect.height) };
}
export function floatingPlacement(rect: ToolbarRect, bounds: ToolbarRect, previous?: ToolbarPlacement): ToolbarPlacement {
    const b = floatingBounds(bounds);
    return { kind: 'floating', x: b.width > rect.width ? clamp(rect.x - b.x, b.width - rect.width) / (b.width - rect.width) : previous?.kind === 'floating' ? previous.x : .5,
        y: b.height > rect.height ? clamp(rect.y - b.y, b.height - rect.height) / (b.height - rect.height) : previous?.kind === 'floating' ? previous.y : 1 };
}
export function toolbarLayout(bounds: ToolbarRect, placement: ToolbarPlacement, size: ToolbarSize, thickness: number) {
    const edge = toolbarEdge(placement);
    const vertical = isVerticalToolbar(edge);
    const video = { ...bounds };
    const rail = { ...bounds };
    if (edge === 'top' || edge === 'bottom') {
        rail.height = Math.min(thickness, bounds.height);
        video.height = Math.max(0, bounds.height - thickness);
        if (edge === 'top') video.y += rail.height;
        else rail.y += video.height;
    } else if (edge) {
        rail.width = Math.min(thickness, bounds.width);
        video.width = Math.max(0, bounds.width - thickness);
        if (edge === 'left') video.x += rail.width;
        else rail.x += video.width;
    }
    const b = edge ? rail : floatingBounds(bounds);
    const bar = { x: b.x, y: b.y, width: Math.min(size.width, b.width), height: Math.min(size.height, b.height) };
    if (placement.kind === 'floating') {
        bar.x += placement.x * Math.max(0, b.width - bar.width);
        bar.y += placement.y * Math.max(0, b.height - bar.height);
    } else if (vertical) bar.y += placement.along * Math.max(0, b.height - bar.height);
    else bar.x += placement.along * Math.max(0, b.width - bar.width);
    return { edge, vertical, video, rail: edge ? rail : null, bar };
}

function edgeDistance(point: ToolbarPoint, edge: ToolbarEdge, b: ToolbarRect) {
    const vertical = isVerticalToolbar(edge);
    const x = vertical ? (edge === 'left' ? b.x : b.x + b.width) : b.x + clamp(point.x - b.x, b.width);
    const y = vertical ? b.y + clamp(point.y - b.y, b.height) : (edge === 'top' ? b.y : b.y + b.height);
    return Math.hypot(point.x - x, point.y - y);
}
export function toolbarCandidate(point: ToolbarPoint, bounds: ToolbarRect, current: ToolbarEdge | null, touch: boolean, thickness: number) {
    const eligible = (edge: ToolbarEdge) => isVerticalToolbar(edge) ? bounds.width > thickness && bounds.height >= thickness
        : bounds.height > thickness && bounds.width >= thickness;
    if (current && eligible(current) && edgeDistance(point, current, bounds) <= (touch ? 48 : 36)) return current;
    let candidate: ToolbarEdge | null = null;
    let best = touch ? 32 : 24;
    for (const edge of TOOLBAR_EDGES) {
        const distance = edgeDistance(point, edge, bounds);
        if (eligible(edge) && distance <= best && (candidate === null || distance < best)) { candidate = edge; best = distance; }
    }
    return candidate;
}

/** A drag is a preview transaction; only finish commits a new placement. */
export class ToolbarDrag {
    readonly original: ToolbarPlacement;
    readonly bounds: ToolbarRect;
    readonly bar: ToolbarRect;
    readonly start: ToolbarPoint;
    readonly grab: ToolbarPoint;
    readonly touch: boolean;
    readonly thickness: number;
    moved = false;
    detached: boolean;
    candidate: ToolbarEdge | null = null;
    preview: ToolbarRect;
    point: ToolbarPoint;
    constructor(placement: ToolbarPlacement, bounds: ToolbarRect, bar: ToolbarRect, point: ToolbarPoint, touch: boolean, thickness: number) {
        this.original = placement; this.bounds = bounds; this.bar = bar; this.start = point; this.point = point;
        this.grab = { x: point.x - bar.x, y: point.y - bar.y };
        this.touch = touch; this.thickness = thickness; this.detached = placement.kind === 'floating'; this.preview = bar;
    }
    move(point: ToolbarPoint) {
        this.point = point;
        const dx = point.x - this.start.x, dy = point.y - this.start.y;
        if (!this.moved && Math.hypot(dx, dy) < (this.touch ? 8 : 4)) return;
        this.moved = true;
        const edge = toolbarEdge(this.original);
        const inward = edge === 'top' ? dy : edge === 'bottom' ? -dy : edge === 'left' ? dx : -dx;
        if (!this.detached && inward >= (this.touch ? 48 : 32)) this.detached = true;
        this.preview = constrainToolbar({ ...this.bar, x: this.bar.x + dx, y: this.bar.y + dy }, this.bounds);
        if (!this.detached && edge) {
            if (isVerticalToolbar(edge)) this.preview.x = this.bar.x;
            else this.preview.y = this.bar.y;
            this.candidate = edge;
        } else this.candidate = toolbarCandidate(point, this.bounds, this.candidate, this.touch, this.thickness);
    }
    finish(size: ToolbarSize): ToolbarPlacement {
        if (!this.moved) return this.original;
        if (this.candidate) {
            const vertical = isVerticalToolbar(this.candidate);
            const length = Math.min(Math.max(size.width, size.height), vertical ? this.bounds.height : this.bounds.width);
            const travel = Math.max(0, (vertical ? this.bounds.height : this.bounds.width) - length);
            const position = vertical ? this.point.y - this.grab.y - this.bounds.y : this.point.x - this.grab.x - this.bounds.x;
            const previousAlong = this.original.kind === 'docked' && this.original.edge === this.candidate ? this.original.along : .5;
            return { kind: 'docked', edge: this.candidate, along: travel > 0 ? clamp(position, travel) / travel : previousAlong };
        }
        return floatingPlacement({ x: this.point.x - this.grab.x, y: this.point.y - this.grab.y, ...size }, this.bounds, this.original);
    }
}
