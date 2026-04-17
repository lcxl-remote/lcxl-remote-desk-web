import { useEffect, useState, useRef } from 'react';
import type { RefObject } from 'react';

interface CursorSyncData {
    base64_png: string;
    hotspot_x: number;
    hotspot_y: number;
    visible: boolean;
    shape_id: number;
    screen_width?: number;
    screen_height?: number;
}

export function useCursorSync(
    cursorSyncChannel: RefObject<RTCDataChannel | null>,
    videoRef: RefObject<HTMLVideoElement | null>,
    hasControl: boolean
) {
    const [cursorStyle, setCursorStyle] = useState<string>('default');
    const lastDataRef = useRef<CursorSyncData | null>(null);

    const applyCursor = (data: CursorSyncData | null) => {
        if (!hasControl) {
            setCursorStyle('default');
            return;
        }

        if (!data) {
            setCursorStyle('default');
            return;
        }

        if (!data.visible) {
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

        const scaleX = videoWidth / videoOriginalWidth;
        const scaleY = videoHeight / videoOriginalHeight;
        const videoScale = Math.min(scaleX, scaleY);

        const screenWidth = data.screen_width || videoOriginalWidth;
        const scale = videoScale * (videoOriginalWidth / screenWidth);

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

    // Handle window resize to recalculate cursor size
    useEffect(() => {
        const handleResize = () => {
            applyCursor(lastDataRef.current);
        };

        window.addEventListener('resize', handleResize);
        return () => {
            window.removeEventListener('resize', handleResize);
        };
    }, [hasControl]);

    // Apply cursor when control state changes
    useEffect(() => {
        applyCursor(lastDataRef.current);
    }, [hasControl]);

    return { cursorStyle };
}
