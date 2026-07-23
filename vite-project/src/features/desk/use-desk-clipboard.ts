import { useEffect, useRef, useState, useCallback } from 'react';

// 1MB max for text
const MAX_TEXT_SIZE = 1024 * 1024;
// 25MB max for image
const MAX_IMAGE_SIZE = 25 * 1024 * 1024;

interface UseDeskClipboardProps {
    clipboardChannel: React.RefObject<RTCDataChannel | null> | React.MutableRefObject<RTCDataChannel | null>;
    hasControl: boolean;
    isActive: boolean;
}

interface ClipboardEventData {
    type: 'text' | 'image_start' | 'image_chunk' | 'image_end' | 'error';
    content?: string;
    width?: number;
    height?: number;
    totalBytes?: number;
    chunkCount?: number;
    index?: number;
}

export function useDeskClipboard({ clipboardChannel, hasControl, isActive }: UseDeskClipboardProps) {
    const [clipboardEnabled, setClipboardEnabled] = useState(false);
    const [transferProgress, setTransferProgress] = useState<number | null>(null);
    const [transferStatus, setTransferStatus] = useState<'idle' | 'sending' | 'receiving' | 'error'>('idle');
    const [errorMessage, setErrorMessage] = useState<string | null>(null);
    const [fallbackToast, setFallbackToast] = useState<{ show: boolean, blob?: Blob, text?: string }>({ show: false });

    // Anti-echo state
    const lastWrittenHash = useRef<number | null>(null);

    // Incoming image chunk assembler
    const chunksBuffer = useRef<string[]>([]);
    const incomingImageState = useRef<{ totalChunks: number, totalBytes: number }>({ totalChunks: 0, totalBytes: 0 });

    // Simple string hash function to match typical server-side hashing strategy or just use string match
    const hashString = (str: string) => {
        let hash = 0;
        for (let i = 0; i < str.length; i++) {
            const char = str.charCodeAt(i);
            hash = (hash << 5) - hash + char;
            hash = hash & hash; // Convert to 32bit int
        }
        return hash;
    };

    const clearAssembler = () => {
        chunksBuffer.current = [];
        incomingImageState.current = { totalChunks: 0, totalBytes: 0 };
        setTransferProgress(null);
        setTransferStatus('idle');
    };

    const showErr = (msg: string) => {
        console.warn("[DeskClipboard] Sync Error:", msg);
        setErrorMessage(msg);
        setTransferStatus('error');
        setTimeout(() => {
            setErrorMessage(null);
            setTransferStatus('idle');
        }, 3000);
    };

    // Toggle state
    const toggleClipboard = useCallback(() => {
        console.log(`[DeskClipboard] toggleClipboard called. Context isSecureContext=${window.isSecureContext}`);
        // Only allow toggling if HTTPS or localhost (Secure Context is a MUST for navigator.clipboard)
        if (window.isSecureContext === false) {
            showErr('Clipboard sync requires HTTPS secure context.');
            return;
        }
        setClipboardEnabled(prev => {
            console.log(`[DeskClipboard] Toggling clipboardEnabled setting from ${prev} to ${!prev}`);
            return !prev;
        });
    }, []);

    // Set the flag back to false if control is lost
    useEffect(() => {
        console.log(`[DeskClipboard] Auth state changed: hasControl=${hasControl}, isActive=${isActive}`);
        if (!hasControl || !isActive) {
            console.log(`[DeskClipboard] disabling...`);
            setClipboardEnabled(false);
        }
    }, [hasControl, isActive]);

    // Handle incoming data channel messages
    useEffect(() => {
        const channel = clipboardChannel?.current;
        if (!channel || !clipboardEnabled) return;

        const handleMessage = async (e: MessageEvent) => {
            try {
                const data: ClipboardEventData = JSON.parse(e.data);
                if (data.type === 'error') {
                    showErr(`Remote Error: ${data.content}`);
                    clearAssembler();
                    return;
                }

                if (data.type === 'text' && data.content) {
                    if (data.content.length > MAX_TEXT_SIZE) {
                        showErr('Incoming text exceeds size limit (1MB). Dropped.');
                        return;
                    }
                    const textHash = hashString(data.content);
                    if (lastWrittenHash.current === textHash) {
                        console.log("Ignoring echoed text");
                        return; // Ignore echo
                    }

                    try {
                        await navigator.clipboard.writeText(data.content);
                        lastWrittenHash.current = textHash;
                    } catch (writeErr: any) {
                        console.warn("Failed to write text to local clipboard, might require user gesture:", writeErr);
                        if (writeErr.name === 'NotAllowedError') {
                            setFallbackToast({ show: true, text: data.content });
                        }
                    }
                } else if (data.type === 'image_start') {
                    const size = data.totalBytes || 0;
                    if (size > MAX_IMAGE_SIZE) {
                        showErr('Incoming image exceeds size limit (25MB). Dropped.');
                        return;
                    }
                    clearAssembler();
                    incomingImageState.current = { totalChunks: data.chunkCount || 0, totalBytes: size };
                    setTransferStatus('receiving');
                    setTransferProgress(0);
                } else if (data.type === 'image_chunk' && data.content) {
                    if (transferStatus !== 'receiving') return; // Skip if no active transfer
                    if (data.index !== undefined) {
                        chunksBuffer.current[data.index] = data.content;
                        const pct = Math.round((chunksBuffer.current.filter(Boolean).length / incomingImageState.current.totalChunks) * 100);
                        setTransferProgress(pct);
                    }
                } else if (data.type === 'image_end') {
                    // Processing image
                    setTransferProgress(100);

                    try {
                        const base64Str = chunksBuffer.current.join('');
                        // Reconstruct byte array
                        const binaryStr = atob(base64Str);
                        const len = binaryStr.length;
                        const bytes = new Uint8Array(len);
                        for (let i = 0; i < len; i++) {
                            bytes[i] = binaryStr.charCodeAt(i);
                        }
                        const blob = new Blob([bytes], { type: 'image/png' });

                        try {
                            // ClipboardItem might not be supported everywhere, wrap it 
                            const item = new ClipboardItem({ 'image/png': blob });
                            await navigator.clipboard.write([item]);
                            // We don't hash image client-side easily right now, but we can set a dummy mark
                            lastWrittenHash.current = Date.now();
                            clearAssembler();
                        } catch (writeErr: any) {
                            console.warn("Failed to write image to local clipboard:", writeErr);
                            if (writeErr.name === 'NotAllowedError') {
                                setFallbackToast({ show: true, blob: blob });
                            }
                            clearAssembler();
                        }
                    } catch (assembleErr) {
                        showErr('Failed to assemble or decode remote image.');
                        clearAssembler();
                    }
                }
            } catch (err) {
                console.error("Failed to handle incoming clipboard data", err);
            }
        };

        channel.addEventListener('message', handleMessage);
        return () => {
            channel.removeEventListener('message', handleMessage);
        };
    }, [clipboardChannel, clipboardEnabled, transferStatus]);


    // Handle local Outbound sync (Local -> Remote)
    useEffect(() => {
        const channel = clipboardChannel?.current;
        if (!clipboardEnabled || !channel || channel.readyState !== 'open') return;

        let syncTimer: any = null;

        const syncLocalToRemote = async () => {
            try {
                if (!navigator.clipboard || !navigator.clipboard.read) return;

                let clipboardItems;
                try {
                    clipboardItems = await navigator.clipboard.read();
                } catch {
                    // console.warn("Could not read clipboard. Maybe no permission?", e);
                    return;
                }

                if (!clipboardItems || clipboardItems.length === 0) return;

                const item = clipboardItems[0];

                // Prioritize Text
                if (item.types.includes('text/plain')) {
                    const blob = await item.getType('text/plain');
                    const text = await blob.text();
                    if (!text) return;
                    const textHash = hashString(text);
                    if (lastWrittenHash.current === textHash) {
                        return;
                    }

                    if (text.length > MAX_TEXT_SIZE) {
                        showErr('Local text is too large to sync (>1MB).');
                        return;
                    }

                    const msg: ClipboardEventData = { type: 'text', content: text };
                    channel.send(JSON.stringify(msg));
                    lastWrittenHash.current = textHash;
                    return;
                }

                // Then Image
                const imageType = item.types.find(type => type.startsWith('image/'));
                if (imageType) {
                    const blob = await item.getType(imageType);
                    const totalBytes = blob.size;

                    if (totalBytes > MAX_IMAGE_SIZE) {
                        showErr('Local image is too large to sync (>25MB).');
                        return;
                    }

                    setTransferStatus('sending');
                    setTransferProgress(0);

                    let buffer: ArrayBuffer;
                    if (blob.arrayBuffer) {
                        buffer = await blob.arrayBuffer();
                    } else {
                        buffer = await new Promise((resolve) => {
                            const reader = new FileReader();
                            reader.onload = () => resolve(reader.result as ArrayBuffer);
                            reader.readAsArrayBuffer(blob);
                        });
                    }

                    const uint8Array = new Uint8Array(buffer);
                    let binary = '';
                    const CHUNK_LEN = 0x8000;
                    for (let i = 0; i < uint8Array.length; i += CHUNK_LEN) {
                        const uarr = uint8Array.subarray(i, i + CHUNK_LEN);
                        binary += String.fromCharCode.apply(null, uarr as unknown as number[]);
                    }
                    const base64Data = btoa(binary);

                    const chunkSize = 32 * 1024; // 32KB
                    const totalChunks = Math.ceil(base64Data.length / chunkSize);

                    // Send start
                    const startMsg: ClipboardEventData = {
                        type: 'image_start',
                        totalBytes: totalBytes,
                        chunkCount: totalChunks
                    };
                    channel.send(JSON.stringify(startMsg));

                    for (let i = 0; i < totalChunks; i++) {
                        const start = i * chunkSize;
                        const end = Math.min(start + chunkSize, base64Data.length);
                        const chunkStr = base64Data.substring(start, end);
                        const chunkMsg: ClipboardEventData = {
                            type: 'image_chunk',
                            index: i,
                            content: chunkStr,
                        };
                        channel.send(JSON.stringify(chunkMsg));
                        setTransferProgress(Math.round(((i + 1) / totalChunks) * 100));

                        // Yield thread a tiny bit on large sends
                        if (i % 10 === 0) {
                            await new Promise(r => setTimeout(r, 1));
                        }
                    }

                    // Send end
                    channel.send(JSON.stringify({ type: 'image_end' }));
                    setTransferProgress(100);
                    setTimeout(() => {
                        setTransferStatus('idle');
                        setTransferProgress(null);
                    }, 500);

                    lastWrittenHash.current = Date.now();
                }

            } catch (err) {
                console.warn("Error in intercepting copy/cut:", err);
            }
        };

        const handleCopyCut = () => {
            if (syncTimer) clearTimeout(syncTimer);
            syncTimer = setTimeout(syncLocalToRemote, 50); // Small delay to let system clip register
        };

        const handleFocusOrInteraction = () => {
            if (syncTimer) clearTimeout(syncTimer);
            syncTimer = setTimeout(syncLocalToRemote, 150);
        };

        const handleKeyDownCapture = (e: KeyboardEvent) => {
            if ((e.ctrlKey || e.metaKey) && e.code === "KeyV") {
                if (syncTimer) clearTimeout(syncTimer);
                syncLocalToRemote();
            }
        };

        document.addEventListener('copy', handleCopyCut);
        document.addEventListener('cut', handleCopyCut);
        window.addEventListener('focus', handleFocusOrInteraction);
        document.addEventListener('keydown', handleKeyDownCapture, { capture: true });

        return () => {
            document.removeEventListener('copy', handleCopyCut);
            document.removeEventListener('cut', handleCopyCut);
            window.removeEventListener('focus', handleFocusOrInteraction);
            document.removeEventListener('keydown', handleKeyDownCapture, { capture: true });
            if (syncTimer) clearTimeout(syncTimer);
        };

    }, [clipboardEnabled, clipboardChannel]);

    const execFallbackToastAction = async () => {
        try {
            if (fallbackToast.text) {
                await navigator.clipboard.writeText(fallbackToast.text);
            } else if (fallbackToast.blob) {
                const it = new ClipboardItem({ 'image/png': fallbackToast.blob });
                await navigator.clipboard.write([it]);
            }
            // update hash
            lastWrittenHash.current = Date.now();
            setFallbackToast({ show: false });
        } catch {
            showErr("Still failed to write clipboard manually.");
        }
    };

    const closeFallbackToast = () => setFallbackToast({ show: false });

    return {
        clipboardEnabled,
        transferProgress,
        transferStatus,
        errorMessage,
        toggleClipboard,
        fallbackToast,
        execFallbackToastAction,
        closeFallbackToast,
    };
}
