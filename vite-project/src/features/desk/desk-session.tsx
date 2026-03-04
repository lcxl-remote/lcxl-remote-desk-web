import { useEffect, useRef, useState, useCallback } from "react"
import type { MouseEvent as ReactMouseEvent } from "react"
import { useParams, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { Loader2, Folder, Terminal as TerminalIcon, MousePointer2, XSquare, Maximize, Minimize, Settings, Volume2, VolumeX, Power, Keyboard, Activity, ShieldCheck, ShieldOff, Clipboard, ClipboardX } from "lucide-react"
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip"
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover"
import { Slider } from "@/components/ui/slider"

import { Button } from "@/components/ui/button"
import "./desk-session.css"
import { useDeskSignaling } from "./use-desk-signaling"
import { useDeskRTC } from "./use-desk-rtc"
import { useDeskInput } from "./use-desk-input"
import { useDeskClipboard } from "./use-desk-clipboard"
import { DeskConfigDialog } from "./desk-config-dialog"
import type { DeskSettings } from "@/services/types"
import {
    SIGNALING_TYPE_CODE_REQUEST_REMOTE,
    SIGNALING_TYPE_CODE_REQUIRE_CONTROL,
    SIGNALING_TYPE_CODE_CLOSE_CONTROL,
    SIGNALING_TYPE_CODE_ACCEPT_CONTROL,
    SIGNALING_TYPE_CODE_DENY_CONTROL,
    SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS,
    SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN,
    SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED
} from "./constants"

export default function DeskSession() {
    const { id: deskId } = useParams<{ id: string }>()
    const navigate = useNavigate()
    const { t } = useTranslation()

    // Control state
    const [hasControl, setHasControl] = useState(false);
    const [hasRequested, setHasRequested] = useState(false);
    const hasRequestedRef = useRef(false);

    const { isConnected, lastMessage, sendMessage } = useDeskSignaling(deskId || null)

    const handleConnect = useCallback(() => {
        if (deskId && !hasRequestedRef.current) {
            console.log("WebSocket opened, requesting remote session directly:", deskId);
            sendMessage(SIGNALING_TYPE_CODE_REQUEST_REMOTE, { session_id: deskId }, deskId);
            hasRequestedRef.current = true;
            setHasRequested(true);
        }
    }, [deskId, sendMessage]);

    useEffect(() => {
        if (isConnected) {
            handleConnect();
        }
    }, [isConnected, handleConnect]);
    const videoRef = useRef<HTMLVideoElement>(null)
    const videoWrapperRef = useRef<HTMLDivElement>(null)
    const controlBarRef = useRef<HTMLDivElement>(null)

    const [isConfigOpen, setIsConfigOpen] = useState(false);
    const [isVideoReady, setIsVideoReady] = useState(false);
    const [isMuted, setIsMuted] = useState(() => {
        // Safari/iOS requires muted for autoPlay
        return /Mobile|Android|iP(ad|hone)/.test(navigator.userAgent) ? true : false;
    });
    const [audioVolume, setAudioVolume] = useState(100);
    const [isFullscreen, setIsFullscreen] = useState(false);

    // Drag UI state
    const [isDragging, setIsDragging] = useState(false);
    const [dragOffset, setDragOffset] = useState({ x: 0, y: 0 });

    const [showStats, setShowStats] = useState(false);

    // Privacy screen state
    const [isPrivateScreen, setIsPrivateScreen] = useState(false);
    const [isPrivateScreenSupported, setIsPrivateScreenSupported] = useState(true);

    const { remoteStream, initData, connect, mouseChannel, keyboardChannel, mouseMoveChannel, clipboardChannel, isRTCConnected, closeRTC, rtcStats } = useDeskRTC({
        deskId: deskId || null,
        lastMessage,
        sendMessage
    });

    const {
        clipboardEnabled,
        transferProgress,
        transferStatus,
        errorMessage,
        toggleClipboard,
        fallbackToast,
        execFallbackToastAction,
        closeFallbackToast
    } = useDeskClipboard({
        clipboardChannel,
        hasControl: isRTCConnected && hasControl,
        isActive: true
    });

    const { sendKeyboardEvents } = useDeskInput({
        videoRef,
        mouseChannel,
        keyboardChannel,
        mouseMoveChannel,
        isConnected: isRTCConnected && hasControl // Only enable inputs if we have control
    });

    // Handle incoming signaling messages regarding control
    useEffect(() => {
        if (!lastMessage) return;
        const { signaling_type } = lastMessage;

        if (signaling_type === SIGNALING_TYPE_CODE_ACCEPT_CONTROL) {
            console.log("Remote control request ACCEPTED by peer.");
            setHasControl(true);
            videoRef.current?.focus();
        } else if (signaling_type === SIGNALING_TYPE_CODE_DENY_CONTROL) {
            console.log("Remote control request DENIED by peer.");
            setHasControl(false);
        } else if (signaling_type === SIGNALING_TYPE_CODE_CLOSE_CONTROL) {
            console.log("Remote control CLOSED by peer.");
            setHasControl(false);
        } else if (signaling_type === SIGNALING_TYPE_CODE_PRIVATE_SCREEN_STATE_CHANGED) {
            const data = lastMessage.signaling_data;
            if (data) {
                console.log("Private screen state changed:", data);
                setIsPrivateScreen(data.visible ?? false);
                setIsPrivateScreenSupported(data.is_supported ?? true);
                if (data.error_msg) {
                    console.warn("Private screen error:", data.error_msg);
                }
            }
        }
    }, [lastMessage]);

    // Reset requested state if connection drops
    useEffect(() => {
        if (!isConnected) {
            hasRequestedRef.current = false;
            setHasRequested(false);
        }
    }, [isConnected]);

    // Wait for INIT data and show the config dialog
    useEffect(() => {
        if (initData && !isRTCConnected && !document.getElementById("desk-config-dialog")) {
            console.log("Showing config dialog for remote session");
            setIsConfigOpen(true);
        }
    }, [initData, isRTCConnected]);

    // Attach remote stream to video element
    useEffect(() => {
        if (videoRef.current && remoteStream && isRTCConnected) {
            console.log("[Video] WebRTC is connected. Setting srcObject with remoteStream. Tracks:", remoteStream.getTracks().map(t => t.kind));

            // Only assign if it hasn't been assigned yet, to avoid React infinite stream interruption
            if (videoRef.current.srcObject !== remoteStream) {
                videoRef.current.srcObject = remoteStream;
            }

            videoRef.current.play().then(() => {
                console.log("[Video] Successfully started playing implicitly.");
            }).catch(e => {
                console.warn("[Video] Failed to implicitly play video stream on mount, user interaction might be required on this browser (Safari iOS): ", e);
            });
        }
    }, [remoteStream, isRTCConnected]);

    // Debugging video element state
    useEffect(() => {
        if (!isConnected || !videoRef.current) return;
        const interval = setInterval(() => {
            const v = videoRef.current;
            if (v) {
                console.log(`[Video State] readyState: ${v.readyState}, paused: ${v.paused}, muted: ${v.muted}, videoWidth: ${v.videoWidth}, videoHeight: ${v.videoHeight}, srcObject: ${!!v.srcObject}`);
            }
        }, 2000);
        return () => clearInterval(interval);
    }, [isConnected]);

    const handleConfigSubmit = (settings: DeskSettings) => {
        if (isRTCConnected && deskId) {
            console.log("Updating desk settings dynamically...", settings);
            sendMessage(SIGNALING_TYPE_CODE_UPDATE_DESK_SETTINGS, settings, deskId);
        } else {
            connect(settings);
        }
        setIsConfigOpen(false);
    };

    const handleConfigCancel = () => {
        setIsConfigOpen(false);
        navigate(`/desk/${deskId}`);
    };

    const handleRequestControl = () => {
        const requestControlData = {
            accept: !hasControl,
            accept_clipboard_sync: !hasControl, // Auto-request clipboard by default on control
            accept_file_transfer: !hasControl,
        };
        // Auto-enable UI state if asking for control
        if (!hasControl && window.isSecureContext !== false) {
            if (!clipboardEnabled) toggleClipboard();
        }

        console.log(`Sending REQUIRE_CONTROL signaling, requestControlData:`, requestControlData);
        sendMessage(SIGNALING_TYPE_CODE_REQUIRE_CONTROL, requestControlData, deskId);
    };

    const handleDisconnect = () => {
        if (deskId) {
            if (isPrivateScreen) {
                console.log(`Disabling private screen before disconnect`);
                sendMessage(SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN, { enable: false }, deskId);
            }
            sendMessage(SIGNALING_TYPE_CODE_CLOSE_CONTROL, null, deskId);
        }
        closeRTC();
        navigate(`/desk/${deskId}`);
    };

    const handleTogglePrivateScreen = () => {
        if (!deskId) return;
        const newState = !isPrivateScreen;
        console.log(`Toggling private screen: ${newState}`);
        sendMessage(SIGNALING_TYPE_CODE_ENABLE_PRIVATE_SCREEN, { enable: newState }, deskId);
    };

    const handleFullscreen = () => {
        if (!document.fullscreenElement) {
            videoWrapperRef.current?.requestFullscreen().catch(err => {
                console.log(`Error attempting to enable fullscreen: ${err.message}`);
            });
            try {
                (navigator as any).keyboard?.lock(['Escape']);
                console.log("Keyboard lock: ESC key captured");
            } catch (error) {
                console.warn("Failed to lock keyboard:", error);
            }
        } else {
            document.exitFullscreen();
            try {
                (navigator as any).keyboard?.unlock();
                console.log("Keyboard unlock: ESC key released");
            } catch (error) {
                console.warn("Failed to unlock keyboard:", error);
            }
        }
    };

    // Keep fullscreen state in sync
    useEffect(() => {
        const handleFullscreenChange = () => {
            setIsFullscreen(!!document.fullscreenElement);
            // Re-center control bar if it exists
            if (controlBarRef.current) {
                const cb = controlBarRef.current;
                cb.style.transition = 'none';
                cb.style.left = '';
                cb.style.top = '';
                cb.style.bottom = '';
                cb.style.transform = '';
            }
        };
        document.addEventListener('fullscreenchange', handleFullscreenChange);
        return () => document.removeEventListener('fullscreenchange', handleFullscreenChange);
    }, []);

    const handleVolumeChange = (value: number) => {
        if (!videoRef.current) return;
        setAudioVolume(value);
        videoRef.current.volume = value / 100;
        setIsMuted(value === 0);
    };

    const toggleMute = () => {
        if (videoRef.current) {
            const newMuted = !isMuted;
            videoRef.current.muted = newMuted;
            setIsMuted(newMuted);
        }
    };

    // Drag handlers for control bar
    const handleDragStart = (e: ReactMouseEvent) => {
        if (!controlBarRef.current || !videoWrapperRef.current) return;

        setIsDragging(true);
        const controlBar = controlBarRef.current;
        const wrapperRect = videoWrapperRef.current.getBoundingClientRect();
        const controlBarRect = controlBar.getBoundingClientRect();

        setDragOffset({
            x: e.clientX - controlBarRect.left,
            y: e.clientY - controlBarRect.top
        });

        // Disable transition while dragging
        controlBar.style.transition = 'none';
        controlBar.style.transform = 'none';
        controlBar.style.bottom = 'auto';

        controlBar.style.left = `${controlBarRect.left - wrapperRect.left}px`;
        controlBar.style.top = `${controlBarRect.top - wrapperRect.top}px`;
    };

    const handleDrag = useCallback((e: MouseEvent) => {
        if (!isDragging || !controlBarRef.current || !videoWrapperRef.current) return;
        e.preventDefault();

        const controlBar = controlBarRef.current;
        const wrapperRect = videoWrapperRef.current.getBoundingClientRect();

        const screenX = e.clientX - dragOffset.x;
        const screenY = e.clientY - dragOffset.y;

        let newX = screenX - wrapperRect.left;
        let newY = screenY - wrapperRect.top;

        const maxX = wrapperRect.width - controlBar.offsetWidth;
        const maxY = wrapperRect.height - controlBar.offsetHeight;

        newX = Math.max(0, Math.min(newX, maxX));
        newY = Math.max(0, Math.min(newY, maxY));

        controlBar.style.left = `${newX}px`;
        controlBar.style.top = `${newY}px`;
    }, [isDragging, dragOffset]);

    const handleDragEnd = useCallback(() => {
        setIsDragging(false);
        if (controlBarRef.current) {
            controlBarRef.current.style.transition = '';
        }
    }, []);

    useEffect(() => {
        if (isDragging) {
            document.addEventListener('mousemove', handleDrag);
            document.addEventListener('mouseup', handleDragEnd);
        } else {
            document.removeEventListener('mousemove', handleDrag);
            document.removeEventListener('mouseup', handleDragEnd);
        }
        return () => {
            document.removeEventListener('mousemove', handleDrag);
            document.removeEventListener('mouseup', handleDragEnd);
        };
    }, [isDragging, handleDrag, handleDragEnd]);

    // Handle generic window resizing to clamp control bar position
    useEffect(() => {
        const wrapper = videoWrapperRef.current;
        if (!wrapper) return;

        const resizeObserver = new ResizeObserver(() => {
            const controlBar = controlBarRef.current;
            if (!controlBar || controlBar.style.transform !== 'none') return; // Only process if previously dragged

            const wrapperRect = wrapper.getBoundingClientRect();

            // Current set inline style properties (numeric limits)
            const currentLeft = parseFloat(controlBar.style.left) || 0;
            const currentTop = parseFloat(controlBar.style.top) || 0;

            const maxX = wrapperRect.width - controlBar.offsetWidth;
            const maxY = wrapperRect.height - controlBar.offsetHeight;

            // Clamp and adjust
            const newX = Math.max(0, Math.min(currentLeft, maxX));
            const newY = Math.max(0, Math.min(currentTop, maxY));

            controlBar.style.left = `${newX}px`;
            controlBar.style.top = `${newY}px`;
        });

        resizeObserver.observe(wrapper);
        return () => resizeObserver.disconnect();
    }, []);

    return (
        <div className="flex h-full flex-col">
            <DeskConfigDialog
                open={isConfigOpen}
                onOpenChange={setIsConfigOpen}
                initData={initData}
                onSubmit={handleConfigSubmit}
                onCancel={handleConfigCancel}
            />
            <div className="flex items-center justify-between border-b p-4">
                <h2 className="text-lg font-semibold">{t('pages.desk.title', 'Remote Desk')} - {deskId}</h2>
                <div className="flex items-center gap-4">
                    {hasControl && (
                        <div className="text-sm font-medium text-blue-500">Controlling</div>
                    )}
                    <div className="flex items-center gap-2">
                        <div className={`h-2 w-2 rounded-full ${isConnected ? 'bg-green-500' : 'bg-red-500'}`} />
                        <span className="text-sm text-muted-foreground">{isConnected ? 'Connected' : 'Disconnected'}</span>
                    </div>
                </div>
            </div>
            <div className="relative flex-1 bg-black flex items-center justify-center overflow-hidden">
                {!isConnected && (
                    <div className="flex flex-col items-center gap-2 text-white z-50">
                        <Loader2 className="h-8 w-8 animate-spin" />
                        <span>Connecting...</span>
                    </div>
                )}

                <TooltipProvider>
                    <div
                        className="videoWrapper"
                        ref={videoWrapperRef}
                    >
                        <video
                            ref={videoRef}
                            className="videoElement h-full w-full object-contain"
                            autoPlay
                            playsInline
                            muted={isMuted}
                            tabIndex={0}
                            onCanPlay={() => setIsVideoReady(true)}
                        />

                        <div
                            className={`videoPlaceholder ${isVideoReady ? 'hidden' : ''}`}
                            onContextMenu={(e) => { e.preventDefault() }}
                        >
                            <div className="placeholderContent">
                                <span className="artText">LCXL Remote Desk</span>
                            </div>
                        </div>

                        {showStats && isConnected && (
                            <div className="absolute top-4 left-4 z-50 bg-black/60 text-white p-3 rounded-lg text-sm font-mono backdrop-blur-md border border-white/20 select-none min-w-[200px]">
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">FPS:</span>
                                    <span className="font-bold text-green-400">{rtcStats.fps}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">Resolution:</span>
                                    <span className="font-bold text-white">{rtcStats.width}x{rtcStats.height}</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">Bitrate:</span>
                                    <span className="font-bold text-blue-400">{rtcStats.bitrate} kbps</span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">Video Codec:</span>
                                    <span className="font-bold text-purple-400">{rtcStats.videoCodec || 'Unknown'}</span>
                                </div>
                                {rtcStats.audioCodec && (
                                    <div className="flex justify-between gap-4 mb-1">
                                        <span className="text-gray-400">Audio Codec:</span>
                                        <span className="font-bold text-purple-400">{rtcStats.audioCodec}</span>
                                    </div>
                                )}
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">Latency:</span>
                                    <span className={`font-bold ${rtcStats.rtt > 150 ? 'text-red-400' : rtcStats.rtt > 80 ? 'text-yellow-400' : 'text-green-400'}`}>
                                        {rtcStats.rtt} ms
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4 mb-1">
                                    <span className="text-gray-400">Packet Loss:</span>
                                    <span className={`font-bold ${rtcStats.packetLoss > 5 ? 'text-red-400' : rtcStats.packetLoss > 1 ? 'text-yellow-400' : 'text-green-400'}`}>
                                        {rtcStats.packetLoss}%
                                    </span>
                                </div>
                                <div className="flex justify-between gap-4">
                                    <span className="text-gray-400">Network:</span>
                                    <span className="font-bold text-orange-400 uppercase">
                                        {rtcStats.networkType || 'Unknown'}
                                    </span>
                                </div>
                            </div>
                        )}

                        {errorMessage && (
                            <div className="absolute top-16 right-4 z-[60] bg-red-500/90 text-white px-4 py-2 rounded-lg text-sm font-medium shadow-lg backdrop-blur-md animate-in fade-in slide-in-from-top-4">
                                {errorMessage}
                            </div>
                        )}

                        {transferStatus !== 'idle' && transferStatus !== 'error' && (
                            <div className="absolute top-16 right-4 z-[60] bg-black/80 text-white px-4 py-2 rounded-lg text-sm font-medium shadow-lg backdrop-blur-md border border-white/10 flex items-center gap-2 animate-in fade-in slide-in-from-top-4">
                                <Loader2 className="w-4 h-4 animate-spin text-blue-400" />
                                <span>{transferProgress ? `${t('pages.desk.syncing', 'Syncing clipboard...')} ${transferProgress}%` : t('pages.desk.syncing', 'Syncing clipboard...')}</span>
                            </div>
                        )}

                        {fallbackToast.show && (
                            <div className="absolute bottom-24 right-4 z-[60] bg-amber-500/90 text-white p-4 rounded-lg shadow-xl flex flex-col gap-3 min-w-[300px] animate-in slide-in-from-bottom-4 pointer-events-auto">
                                <div className="flex justify-between items-start">
                                    <span className="font-semibold text-sm">Action Required</span>
                                    <button onClick={closeFallbackToast} className="text-white/80 hover:text-white"><XSquare className="w-4 h-4" /></button>
                                </div>
                                <p className="text-xs text-amber-100">{fallbackToast.text || 'Clipboard update received, please click to sync.'}</p>
                                <Button variant="secondary" size="sm" className="w-full bg-white text-amber-900 hover:bg-amber-100" onClick={execFallbackToastAction}>
                                    Sync Now
                                </Button>
                            </div>
                        )}

                        {isConnected && (
                            <div
                                ref={controlBarRef}
                                className="controlBar"
                                onMouseDown={handleDragStart}
                            >
                                <div className="controlButtons">
                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={handleRequestControl}
                                            >
                                                {hasControl ? <XSquare /> : <MousePointer2 />}
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{hasControl ? t('pages.desk.exitControl', 'Exit Control') : t('pages.desk.requestControl', 'Request Control')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={handleFullscreen}
                                            >
                                                {isFullscreen ? <Minimize /> : <Maximize />}
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{isFullscreen ? t('pages.desk.exitFullscreen', 'Exit Fullscreen') : t('pages.desk.fullscreen', 'Fullscreen')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                                onClick={() => setIsConfigOpen(true)}
                                            >
                                                <Settings />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{t('pages.desk.settings', 'Settings')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className={`controlButton ${showStats ? "bg-white/20" : ""}`}
                                                onClick={() => setShowStats(!showStats)}
                                            >
                                                <Activity />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{showStats ? t('pages.desk.hideStats', 'Hide Network Stats') : t('pages.desk.showStats', 'Show Network Stats')}</p>
                                        </TooltipContent>
                                    </Tooltip>

                                    {hasControl && isPrivateScreenSupported && (
                                        <Tooltip>
                                            <TooltipTrigger asChild>
                                                <Button
                                                    variant="ghost"
                                                    className={`controlButton ${isPrivateScreen ? "bg-white/20 text-green-400" : ""}`}
                                                    onClick={handleTogglePrivateScreen}
                                                >
                                                    {isPrivateScreen ? <ShieldCheck /> : <ShieldOff />}
                                                </Button>
                                            </TooltipTrigger>
                                            <TooltipContent>
                                                <p>{isPrivateScreen ? t('pages.desk.disablePrivateScreen', 'Disable Privacy Screen') : t('pages.desk.enablePrivateScreen', 'Enable Privacy Screen')}</p>
                                            </TooltipContent>
                                        </Tooltip>
                                    )}

                                    {hasControl && (
                                        <Tooltip>
                                            <TooltipTrigger asChild>
                                                <Button
                                                    variant="ghost"
                                                    className={`controlButton ${clipboardEnabled ? "bg-white/20 text-blue-400" : "text-white/50"}`}
                                                    onClick={(e) => {
                                                        e.preventDefault();
                                                        console.log("Clipboard Toggle Button Hit");
                                                        if (typeof toggleClipboard === 'function') {
                                                            toggleClipboard();
                                                        } else {
                                                            console.error("toggleClipboard is not a function!");
                                                        }
                                                    }}
                                                >
                                                    {clipboardEnabled ? <Clipboard /> : <ClipboardX />}
                                                </Button>
                                            </TooltipTrigger>
                                            <TooltipContent>
                                                <p>{clipboardEnabled ? t('pages.desk.disableClipboardSync', 'Disable Clipboard Sync') : t('pages.desk.enableClipboardSync', 'Enable Clipboard Sync')} {!window.isSecureContext ? t('pages.desk.clipboardHttpsRequired', '(HTTPS Required)') : ''}</p>
                                            </TooltipContent>
                                        </Tooltip>
                                    )}

                                    {hasControl && (
                                        <DropdownMenu>
                                            <DropdownMenuTrigger asChild>
                                                <Button variant="ghost" className="controlButton">
                                                    <Keyboard />
                                                </Button>
                                            </DropdownMenuTrigger>
                                            <DropdownMenuContent align="end" className="w-56 bg-background/90 backdrop-blur-md border-white/10">
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 17 }, // Ctrl
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 46 }, // Del
                                                        { event: "keyup", keyCode: 46 },
                                                        { event: "keyup", keyCode: 18 },
                                                        { event: "keyup", keyCode: 17 },
                                                    ]);
                                                }}>
                                                    Ctrl + Alt + Del
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 17 }, // Ctrl
                                                        { event: "keydown", keyCode: 16 }, // Shift
                                                        { event: "keydown", keyCode: 27 }, // Esc
                                                        { event: "keyup", keyCode: 27 },
                                                        { event: "keyup", keyCode: 16 },
                                                        { event: "keyup", keyCode: 17 },
                                                    ]);
                                                }}>
                                                    Ctrl + Shift + Esc (任务管理器)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 115 }, // F4
                                                        { event: "keyup", keyCode: 115 },
                                                        { event: "keyup", keyCode: 18 },
                                                    ]);
                                                }}>
                                                    Alt + F4
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 18 }, // Alt
                                                        { event: "keydown", keyCode: 9 }, // Tab
                                                        { event: "keyup", keyCode: 9 },
                                                        { event: "keyup", keyCode: 18 },
                                                    ]);
                                                }}>
                                                    Alt + Tab (切换窗口)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Windows Key
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 68 }, // D
                                                        { event: "keyup", keyCode: 68 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + D (显示桌面)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 69 }, // E
                                                        { event: "keyup", keyCode: 69 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + E (打开资源管理器)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 82 }, // R
                                                        { event: "keyup", keyCode: 82 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + R (运行)
                                                </DropdownMenuItem>
                                                <DropdownMenuItem onClick={() => {
                                                    sendKeyboardEvents([
                                                        { event: "keydown", keyCode: 91 }, // Win
                                                        { event: "keydown", keyCode: 76 }, // L
                                                        { event: "keyup", keyCode: 76 },
                                                        { event: "keyup", keyCode: 91 },
                                                    ]);
                                                }}>
                                                    Win + L (锁定计算机)
                                                </DropdownMenuItem>
                                            </DropdownMenuContent>
                                        </DropdownMenu>
                                    )}

                                    <Popover>
                                        <PopoverTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton"
                                            >
                                                {isMuted || audioVolume === 0 ? <VolumeX /> : <Volume2 />}
                                            </Button>
                                        </PopoverTrigger>
                                        <PopoverContent side="top" className="w-32 px-3 py-4 flex flex-col items-center gap-2 bg-background/90 backdrop-blur-md border-white/10" align="center" sideOffset={16} onOpenAutoFocus={(e) => e.preventDefault()}>
                                            <Slider
                                                min={0}
                                                max={100}
                                                step={1}
                                                value={[audioVolume]}
                                                onValueChange={(vals) => handleVolumeChange(vals[0])}
                                                className="w-full"
                                            />
                                        </PopoverContent>
                                    </Popover>

                                    <div className="w-px h-6 bg-white/20 mx-1" />

                                    <Tooltip>
                                        <TooltipTrigger asChild>
                                            <Button
                                                variant="ghost"
                                                className="controlButton text-red-500 hover:text-red-400 hover:bg-red-500/20"
                                                onClick={handleDisconnect}
                                            >
                                                <Power />
                                            </Button>
                                        </TooltipTrigger>
                                        <TooltipContent>
                                            <p>{t('pages.desk.disconnect', 'Disconnect')}</p>
                                        </TooltipContent>
                                    </Tooltip>
                                </div>
                            </div>
                        )}
                    </div>
                </TooltipProvider>
            </div>
        </div>
    )
}
