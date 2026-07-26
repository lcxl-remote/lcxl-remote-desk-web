import { describe, it, expect } from 'vitest';
import { computeVideoContentRect, computeVideoContentRectInOverlay } from './video-content-rect';

describe('computeVideoContentRect', () => {
    it('fills the box when aspect ratios match', () => {
        expect(computeVideoContentRect(800, 450, 1920, 1080)).toEqual({
            offsetX: 0, offsetY: 0, width: 800, height: 450,
        });
    });

    it('letterboxes a wide frame inside a tall box', () => {
        // 16:9 frame in a 800x600 box -> 800x450 centered vertically
        expect(computeVideoContentRect(800, 600, 1920, 1080)).toEqual({
            offsetX: 0, offsetY: 75, width: 800, height: 450,
        });
    });

    it('pillarboxes a tall frame inside a wide box', () => {
        // 1:1 frame in a 800x400 box -> 400x400 centered horizontally
        expect(computeVideoContentRect(800, 400, 1000, 1000)).toEqual({
            offsetX: 200, offsetY: 0, width: 400, height: 400,
        });
    });

    it('returns null before the first frame is known', () => {
        expect(computeVideoContentRect(800, 600, 0, 0)).toBeNull();
        expect(computeVideoContentRect(0, 0, 1920, 1080)).toBeNull();
    });
});

describe('computeVideoContentRectInOverlay', () => {
    it('keeps the content rect in canvas coordinates when both boxes align', () => {
        const rect = { left: 100, top: 50, width: 800, height: 600 };
        expect(computeVideoContentRectInOverlay(rect, rect, 1920, 1080)).toEqual({
            offsetX: 0, offsetY: 75, width: 800, height: 450,
        });
    });

    it('folds in the video offset within the canvas', () => {
        expect(computeVideoContentRectInOverlay(
            { left: 100, top: 50, width: 800, height: 600 },
            { left: 140, top: 70, width: 720, height: 540 },
            1920, 1080,
        )).toEqual({
            offsetX: 40, offsetY: 20 + 67.5, width: 720, height: 405,
        });
    });

    it('returns null while the video has no frame', () => {
        const rect = { left: 0, top: 0, width: 800, height: 600 };
        expect(computeVideoContentRectInOverlay(rect, rect, 0, 0)).toBeNull();
    });
});
