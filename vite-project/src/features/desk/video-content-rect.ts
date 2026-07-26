/**
 * Geometry of the area actually covered by video pixels inside an
 * `object-fit: contain` <video> element.
 *
 * With `contain`, the frame is uniformly scaled to fit the element box and
 * centered, so unless the element box and the frame share the same aspect
 * ratio there are letterbox (or pillarbox) bars on two sides. Whiteboard
 * coordinates are normalized against this inner rect, not against the element
 * box, so both hit-testing and rendering must use the same geometry.
 */
export type VideoContentRect = {
    offsetX: number;
    offsetY: number;
    width: number;
    height: number;
};

/**
 * Compute the content rect for a frame of `videoWidth` x `videoHeight`
 * displayed inside a `boxWidth` x `boxHeight` element with `object-fit: contain`.
 * Returns null when any dimension is missing or non-positive, which happens
 * before the first frame arrives.
 */
export function computeVideoContentRect(
    boxWidth: number,
    boxHeight: number,
    videoWidth: number,
    videoHeight: number,
): VideoContentRect | null {
    if (!(boxWidth > 0) || !(boxHeight > 0) || !(videoWidth > 0) || !(videoHeight > 0)) {
        return null;
    }
    const scale = Math.min(boxWidth / videoWidth, boxHeight / videoHeight);
    const width = videoWidth * scale;
    const height = videoHeight * scale;
    return {
        offsetX: (boxWidth - width) / 2,
        offsetY: (boxHeight - height) / 2,
        width,
        height,
    };
}

type BoxRect = { left: number; top: number; width: number; height: number };

/**
 * Same content rect, expressed in the coordinate system of an overlay canvas
 * that sits above the video. The canvas usually covers the whole wrapper while
 * the video may be laid out differently, so the video's own offset within the
 * canvas is folded in.
 */
export function computeVideoContentRectInOverlay(
    canvasRect: BoxRect,
    videoRect: BoxRect,
    videoWidth: number,
    videoHeight: number,
): VideoContentRect | null {
    const content = computeVideoContentRect(videoRect.width, videoRect.height, videoWidth, videoHeight);
    if (!content) return null;
    return {
        offsetX: content.offsetX + (videoRect.left - canvasRect.left),
        offsetY: content.offsetY + (videoRect.top - canvasRect.top),
        width: content.width,
        height: content.height,
    };
}
