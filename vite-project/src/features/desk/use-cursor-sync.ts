import { useEffect, useState, useRef } from 'react';
import type { RefObject } from 'react';
import { useToast } from '@/hooks/use-toast';
import { useTranslation } from 'react-i18next';

interface CursorSyncData {
    base64_png: string;
    hotspot_x: number;
    hotspot_y: number;
    visible: boolean;
    shape_id: number;
    screen_width?: number;
    screen_height?: number;
    /// Set by the backend when the OS has composited the cursor into
    /// the captured frame (DXGI software-cursor mode). Tells the
    /// front-end to hide its local CSS cursor and trust the video
    /// stream's baked-in cursor.
    embedded?: boolean;
}

/**
 * Compute the CSS pixel scale that should be applied to a remote
 * cursor PNG before it is rendered as the local CSS cursor sprite.
 *
 * The remote cursor is captured at OS-pixel resolution (e.g. 24×24)
 * relative to a back-end "screen" of `screen` size. The browser
 * receives a video stream whose intrinsic dimensions are
 * `videoNative` and renders it inside a DOM element of `videoDom`
 * size. So the cursor must be scaled twice:
 *
 * 1. encoder ratio (`videoNative / screen`) — corrects for the
 *    backend possibly down-sampling the capture before encoding
 *    (current pipeline runs 1:1 but we keep the safety factor so
 *    a future encoder change does not regress cursor scaling).
 * 2. DOM ratio (`videoDom / videoNative`) — corrects for the
 *    browser fitting the video into the page (CSS `object-fit`
 *    semantics).
 *
 * Both ratios use the conservative `min(width-ratio, height-ratio)`
 * so the cursor never overshoots when the aspect ratio differs
 * between source and target (letterbox / pillarbox / a future
 * portrait monitor).
 *
 * Returns 0 when any dimension is zero (e.g. before the first
 * frame has decoded); callers should treat 0 as "skip drawing".
 */
export function computeCursorScale(
    videoDom: { width: number; height: number },
    videoNative: { width: number; height: number },
    screen: { width: number; height: number },
): number {
    if (videoNative.width === 0 || videoNative.height === 0) return 0;
    if (screen.width === 0 || screen.height === 0) return 0;
    const videoScale = Math.min(
        videoDom.width / videoNative.width,
        videoDom.height / videoNative.height,
    );
    const encoderRatio = Math.min(
        videoNative.width / screen.width,
        videoNative.height / screen.height,
    );
    return videoScale * encoderRatio;
}

export function useCursorSync(
    cursorSyncChannel: RefObject<RTCDataChannel | null>,
    videoRef: RefObject<HTMLVideoElement | null>,
    hasControl: boolean
) {
    const [cursorStyle, setCursorStyle] = useState<string>('default');
    const lastDataRef = useRef<CursorSyncData | null>(null);
    const lastEmbeddedRef = useRef<boolean>(false);
    const { toast } = useToast();
    const { t } = useTranslation();

    const applyCursor = (data: CursorSyncData | null) => {
        // Detect a hardware→software cursor mode transition so the
        // user gets one heads-up that the second cursor in the
        // video is the OS baking its cursor into the frame (not a
        // bug). Only fire on the rising edge — sustained embedded
        // mode would otherwise spam every cursor data tick.
        const wasEmbedded = lastEmbeddedRef.current;
        const isEmbedded = !!data?.embedded;
        if (hasControl && isEmbedded && !wasEmbedded) {
            toast({
                title: t(
                    'pages.desk.remoteCursorActive.title',
                    'Remote cursor visible in frame',
                ),
                description: t(
                    'pages.desk.remoteCursorActive.description',
                    'Display driver limitation: the OS cursor is baked into the remote video, so you may see two cursors. The local one stays responsive.',
                ),
                duration: 5000,
            });
        }
        lastEmbeddedRef.current = isEmbedded;

        if (!hasControl) {
            setCursorStyle('default');
            return;
        }

        if (!data) {
            setCursorStyle('default');
            return;
        }

        if (!data.visible) {
            if (data.embedded) {
                // Software-cursor (DXGI) mode: the backend reports
                // `visible=false` because DXGI thinks the pointer
                // plane is gone, but the OS has actually composited
                // the cursor into the desktop frame instead. The
                // user is still moving a real cursor — we just must
                // not treat this as "cursor disappeared" and hide
                // the local sprite. Leave the previous cursor style
                // untouched so the low-latency local sprite keeps
                // tracking the user's mouse; the user sees two
                // cursors but the local one stays responsive.
                return;
            }
            // Genuine hidden-cursor state (IME entry, cursor
            // confined, etc.) — let the page CSS show no cursor.
            setCursorStyle('none');
            return;
        }

        const video = videoRef.current;
        if (!video) return;

        const videoWidth = video.clientWidth;
        const videoHeight = video.clientHeight;
        const videoOriginalWidth = video.videoWidth;
        const videoOriginalHeight = video.videoHeight;

        if (videoOriginalWidth === 0 || videoOriginalHeight === 0) return;

        const screenWidth = data.screen_width || videoOriginalWidth;
        const screenHeight = data.screen_height || videoOriginalHeight;

        const scale = computeCursorScale(
            { width: videoWidth, height: videoHeight },
            { width: videoOriginalWidth, height: videoOriginalHeight },
            { width: screenWidth, height: screenHeight },
        );
        if (scale === 0) return;

        const img = new Image();
        img.onload = () => {
            const scaledWidth = Math.max(1, Math.round(img.width * scale));
            const scaledHeight = Math.max(1, Math.round(img.height * scale));

            const canvas = document.createElement('canvas');
            canvas.width = scaledWidth;
            canvas.height = scaledHeight;

            const ctx = canvas.getContext('2d');
            if (!ctx) return;

            ctx.drawImage(img, 0, 0, scaledWidth, scaledHeight);
            const scaledDataUrl = canvas.toDataURL('image/png');

            const scaledHotspotX = Math.round(data.hotspot_x * scale);
            const scaledHotspotY = Math.round(data.hotspot_y * scale);

            setCursorStyle(`url(${scaledDataUrl}) ${scaledHotspotX} ${scaledHotspotY}, auto`);
        };
        img.src = `data:image/png;base64,${data.base64_png}`;
    };

    useEffect(() => {
        const channel = cursorSyncChannel.current;
        if (!channel) return;

        const handleMessage = (event: MessageEvent) => {
            try {
                const data: CursorSyncData = JSON.parse(event.data);
                lastDataRef.current = data;
                applyCursor(data);
            } catch (err) {
                console.error('Failed to parse cursor sync data:', err);
            }
        };

        channel.addEventListener('message', handleMessage);

        return () => {
            channel.removeEventListener('message', handleMessage);
        };
    }, [cursorSyncChannel, hasControl]);

    // Recompute cursor scale when the video element resizes in the
    // page (CSS reflow, sidebar toggle, etc.).
    useEffect(() => {
        const handleResize = () => {
            applyCursor(lastDataRef.current);
        };

        window.addEventListener('resize', handleResize);
        return () => {
            window.removeEventListener('resize', handleResize);
        };
    }, [hasControl]);

    // Recompute cursor scale when the *video stream's intrinsic*
    // dimensions change — e.g. the remote desktop switched
    // resolution mid-session and the next IDR carried new SPS.
    // HTMLMediaElement's standard `resize` event fires exactly when
    // videoWidth/videoHeight change. The `window.resize` listener
    // above does not cover this path because the DOM container size
    // may stay the same while only the video intrinsic size moves.
    useEffect(() => {
        const video = videoRef.current;
        if (!video) return;
        const handleVideoResize = () => {
            applyCursor(lastDataRef.current);
        };
        video.addEventListener('resize', handleVideoResize);
        return () => {
            video.removeEventListener('resize', handleVideoResize);
        };
    }, [videoRef, hasControl]);

    // Apply cursor when control state changes
    useEffect(() => {
        applyCursor(lastDataRef.current);
    }, [hasControl]);

    return { cursorStyle };
}
