import { describe, it, expect, vi } from 'vitest';
import { renderElements } from './whiteboard-canvas';
import type { WhiteboardElement } from './use-desk-whiteboard';

function fakeContext() {
    return {
        clearRect: vi.fn(),
        beginPath: vi.fn(),
        moveTo: vi.fn(),
        lineTo: vi.fn(),
        stroke: vi.fn(),
        fillText: vi.fn(),
        strokeStyle: '',
        fillStyle: '',
        lineWidth: 0,
        lineCap: '',
        lineJoin: '',
        font: '',
    } as unknown as CanvasRenderingContext2D & Record<string, ReturnType<typeof vi.fn>>;
}

const stroke: WhiteboardElement = {
    id: 'a',
    type: 'draw',
    tool: 'pen',
    points: [{ x: 0, y: 0 }, { x: 1, y: 1 }],
    color: '#ff0000',
    width: 3,
};

describe('renderElements', () => {
    it('spans the whole canvas without a target rect', () => {
        const ctx = fakeContext();
        renderElements(ctx, [stroke], 800, 600);
        expect(ctx.moveTo).toHaveBeenCalledWith(0, 0);
        expect(ctx.lineTo).toHaveBeenCalledWith(800, 600);
    });

    it('maps normalized points into the letterboxed video rect', () => {
        const ctx = fakeContext();
        // 16:9 video inside an 800x600 canvas -> 800x450 with 75px bars
        renderElements(ctx, [stroke], 800, 600, { offsetX: 0, offsetY: 75, width: 800, height: 450 });
        expect(ctx.clearRect).toHaveBeenCalledWith(0, 0, 800, 600);
        expect(ctx.moveTo).toHaveBeenCalledWith(0, 75);
        expect(ctx.lineTo).toHaveBeenCalledWith(800, 525);
    });

    it('maps text anchors into the target rect too', () => {
        const ctx = fakeContext();
        const text: WhiteboardElement = {
            id: 'b', type: 'text', x: 0.5, y: 0.5, content: 'hi', color: '#fff', fontSize: 24,
        };
        renderElements(ctx, [text], 800, 600, { offsetX: 200, offsetY: 0, width: 400, height: 600 });
        expect(ctx.fillText).toHaveBeenCalledWith('hi', 400, 300);
    });
});
