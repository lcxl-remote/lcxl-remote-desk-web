import { useState } from "react"
import type {
    MouseEvent as ReactMouseEvent,
    RefObject,
} from "react"
import { useTranslation } from "react-i18next"
import {
    Activity,
    Clipboard,
    ClipboardX,
    Keyboard,
    Loader2,
    Lock,
    Maximize,
    Menu,
    Mic,
    MicOff,
    Minimize,
    MousePointer2,
    PenTool,
    Power,
    Settings,
    ShieldCheck,
    ShieldOff,
    Sparkles,
    Volume2,
    VolumeX,
    XSquare,
} from "lucide-react"

import { Button } from "@/components/ui/button"
import type { OperationSystemEnum } from "@/services/types"
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import {
    Popover,
    PopoverContent,
    PopoverTrigger,
} from "@/components/ui/popover"
import { Slider } from "@/components/ui/slider"
import {
    Tooltip,
    TooltipContent,
    TooltipTrigger,
} from "@/components/ui/tooltip"
import type { RestrictedSession } from "./restricted-session"
import type { useDeskInput } from "./use-desk-input"
import type { useDeskMicrophone } from "./use-desk-microphone"
import type { useDeskWhiteboard } from "./use-desk-whiteboard"
import { getKeyboardShortcuts } from "./keyboard-shortcuts"

type DeskControlBarProps = {
    audioVolume: number
    clipboardEnabled: boolean
    controlBarRef: RefObject<HTMLDivElement | null>
    hasControl: boolean
    isDragging: boolean
    isFullscreen: boolean
    isMuted: boolean
    isPrivateScreen: boolean
    isPrivateScreenSupported: boolean
    isWaitingApproval: boolean
    keyboardLockSupported: boolean
    microphone: ReturnType<typeof useDeskMicrophone>
    onChangeVolume: (value: number) => void
    onDisconnect: () => void
    onDragStart: (event: ReactMouseEvent) => void
    onOpenSettings: () => void
    onRequestControl: () => void
    onSendKeyboardEvents: ReturnType<typeof useDeskInput>["sendKeyboardEvents"]
    onToggleClipboard: () => void
    onToggleFullscreen: () => void
    onTogglePrivateScreen: () => void
    operationSystem?: OperationSystemEnum
    restricted: RestrictedSession
    setShowDiagnose: (show: boolean) => void
    setShowStats: (show: boolean) => void
    showDiagnose: boolean
    showStats: boolean
    whiteboard: ReturnType<typeof useDeskWhiteboard>
}

export function DeskControlBar({
    audioVolume,
    clipboardEnabled,
    controlBarRef,
    hasControl,
    isDragging,
    isFullscreen,
    isMuted,
    isPrivateScreen,
    isPrivateScreenSupported,
    isWaitingApproval,
    keyboardLockSupported,
    microphone,
    onChangeVolume,
    onDisconnect,
    onDragStart,
    onOpenSettings,
    onRequestControl,
    onSendKeyboardEvents,
    onToggleClipboard,
    onToggleFullscreen,
    onTogglePrivateScreen,
    operationSystem,
    restricted,
    setShowDiagnose,
    setShowStats,
    showDiagnose,
    showStats,
    whiteboard,
}: DeskControlBarProps) {
    const { t } = useTranslation()
    const [isHovered, setIsHovered] = useState(false)
    const [isMenuOpen, setIsMenuOpen] = useState(false)
    const [isDiagnoseHovered, setIsDiagnoseHovered] = useState(false)
    const isExpanded = isHovered || isMenuOpen || isDragging

    return (
        <div
            className="controlBar"
            onMouseEnter={() => setIsHovered(true)}
            onMouseLeave={() => setIsHovered(false)}
            ref={controlBarRef}
        >
            <div
                className="controlBarDragHandle"
                onMouseDown={onDragStart}
                onTouchStart={() => setIsHovered((hovered) => !hovered)}
            >
                <Menu className="h-5 w-5" />
            </div>
            <div
                className={`controlBarContent ${isExpanded ? "expanded" : "collapsed"}`}
                inert={!isExpanded ? true : undefined}
                onBlur={(event) => {
                    if (!controlBarRef.current?.contains(event.relatedTarget as Node)) {
                        setIsHovered(false)
                    }
                }}
                onFocus={() => setIsHovered(true)}
            >
                <div className="controlButtons">
                    {restricted.isRestricted && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <span
                                    aria-label={t("pages.desk.restricted.indicator")}
                                    className="controlButton flex cursor-default items-center justify-center text-amber-400"
                                >
                                    <Lock />
                                </span>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>{t("pages.desk.restricted.indicator")}</p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    {restricted.capabilityVisible("allow_remote_control") && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className="controlButton"
                                    disabled={isWaitingApproval}
                                    onClick={onRequestControl}
                                    variant="ghost"
                                >
                                    {isWaitingApproval
                                        ? <Loader2 className="animate-spin" />
                                        : hasControl ? <XSquare /> : <MousePointer2 />}
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>
                                    {isWaitingApproval
                                        ? t("pages.desk.waitingPermission")
                                        : hasControl
                                            ? t("pages.desk.exitControl")
                                            : t("pages.desk.requestControl")}
                                </p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    <Tooltip>
                        <TooltipTrigger asChild>
                            <Button
                                className="controlButton"
                                onClick={onToggleFullscreen}
                                variant="ghost"
                            >
                                {isFullscreen ? <Minimize /> : <Maximize />}
                            </Button>
                        </TooltipTrigger>
                        <TooltipContent>
                            <p>{isFullscreen ? t("pages.desk.exitFullscreen") : t("pages.desk.fullscreen")}</p>
                        </TooltipContent>
                    </Tooltip>

                    {restricted.ownerPlaneVisible && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className="controlButton"
                                    onClick={onOpenSettings}
                                    variant="ghost"
                                >
                                    <Settings />
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>{t("pages.desk.settings")}</p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    <Tooltip>
                        <TooltipTrigger asChild>
                            <Button
                                className={`controlButton ${showStats ? "bg-white/20" : ""}`}
                                onClick={() => setShowStats(!showStats)}
                                variant="ghost"
                            >
                                <Activity />
                            </Button>
                        </TooltipTrigger>
                        <TooltipContent>
                            <p>{showStats ? t("pages.desk.hideStats") : t("pages.desk.showStats")}</p>
                        </TooltipContent>
                    </Tooltip>

                    <svg
                        aria-hidden="true"
                        className="absolute h-0 w-0"
                        height="0"
                        width="0"
                    >
                        <defs>
                            <linearGradient id="ai-rainbow-gradient" x1="0%" x2="100%" y1="0%" y2="100%">
                                <stop offset="0%" stopColor="#3b82f6">
                                    <animate attributeName="stop-color" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" values="#3b82f6; #8b5cf6; #d946ef; #f43f5e; #3b82f6" />
                                </stop>
                                <stop offset="33%" stopColor="#8b5cf6">
                                    <animate attributeName="stop-color" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" values="#8b5cf6; #d946ef; #f43f5e; #3b82f6; #8b5cf6" />
                                </stop>
                                <stop offset="66%" stopColor="#d946ef">
                                    <animate attributeName="stop-color" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" values="#d946ef; #f43f5e; #3b82f6; #8b5cf6; #d946ef" />
                                </stop>
                                <stop offset="100%" stopColor="#f43f5e">
                                    <animate attributeName="stop-color" dur={isDiagnoseHovered ? "0.8s" : "4s"} repeatCount="indefinite" values="#f43f5e; #3b82f6; #8b5cf6; #d946ef; #f43f5e" />
                                </stop>
                            </linearGradient>
                        </defs>
                    </svg>

                    {restricted.ownerPlaneVisible && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className={`controlButton ${showDiagnose ? "bg-white/20" : ""}`}
                                    onClick={() => setShowDiagnose(!showDiagnose)}
                                    onMouseEnter={() => setIsDiagnoseHovered(true)}
                                    onMouseLeave={() => setIsDiagnoseHovered(false)}
                                    variant="ghost"
                                >
                                    <Sparkles style={{ stroke: "url(#ai-rainbow-gradient)" }} />
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>{showDiagnose ? t("pages.desk.diagnose.hidePanel") : t("pages.desk.diagnose.showPanel")}</p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    {hasControl
                        && isPrivateScreenSupported
                        && restricted.capabilityVisible("allow_private_screen") && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className={`controlButton ${isPrivateScreen ? "bg-white/20 text-green-400" : ""}`}
                                    onClick={onTogglePrivateScreen}
                                    variant="ghost"
                                >
                                    {isPrivateScreen ? <ShieldCheck /> : <ShieldOff />}
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>{isPrivateScreen ? t("pages.desk.disablePrivateScreen") : t("pages.desk.enablePrivateScreen")}</p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    {hasControl && restricted.capabilityVisible("allow_clipboard_sync") && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className={`controlButton ${clipboardEnabled ? "bg-white/20 text-blue-400" : "text-white/50"}`}
                                    onClick={(event) => {
                                        event.preventDefault()
                                        onToggleClipboard()
                                    }}
                                    variant="ghost"
                                >
                                    {clipboardEnabled ? <Clipboard /> : <ClipboardX />}
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>
                                    {clipboardEnabled
                                        ? t("pages.desk.disableClipboardSync")
                                        : t("pages.desk.enableClipboardSync")}
                                    {!window.isSecureContext
                                        ? ` ${t("pages.desk.clipboardHttpsRequired")}`
                                        : ""}
                                </p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    {hasControl && (
                        <DropdownMenu>
                            <DropdownMenuTrigger asChild>
                                <Button className="controlButton" variant="ghost">
                                    <Keyboard />
                                </Button>
                            </DropdownMenuTrigger>
                            <DropdownMenuContent
                                align="end"
                                className="w-56 border-white/10 bg-background/90 backdrop-blur-md"
                            >
                                {getKeyboardShortcuts(operationSystem, {
                                    includeEscape: !keyboardLockSupported,
                                }).map((shortcut) => (
                                    <DropdownMenuItem
                                        key={shortcut.id}
                                        onClick={() => onSendKeyboardEvents(shortcut.events)}
                                    >
                                        {t(shortcut.labelKey)}
                                    </DropdownMenuItem>
                                ))}
                            </DropdownMenuContent>
                        </DropdownMenu>
                    )}

                    {restricted.capabilityVisible("allow_whiteboard") && (
                        <Tooltip>
                            <TooltipTrigger asChild>
                                <Button
                                    className={`controlButton ${whiteboard.isActive ? "text-yellow-400" : ""}`}
                                    disabled={!whiteboard.canActivate}
                                    onClick={whiteboard.toggleWhiteboard}
                                    variant="ghost"
                                >
                                    <PenTool />
                                </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                                <p>
                                    {whiteboard.canActivate
                                        ? whiteboard.isActive
                                            ? t("pages.desk.closeWhiteboard")
                                            : t("pages.desk.openWhiteboard")
                                        : t("pages.desk.whiteboardUnavailable")}
                                </p>
                            </TooltipContent>
                        </Tooltip>
                    )}

                    <Tooltip>
                        <TooltipTrigger asChild>
                            <Button
                                className={`controlButton ${microphone.isMicActive ? "text-green-400" : ""} ${microphone.micError ? "text-red-400" : ""}`}
                                onClick={microphone.toggleMicrophone}
                                variant="ghost"
                            >
                                {microphone.isMicActive ? <Mic /> : <MicOff />}
                            </Button>
                        </TooltipTrigger>
                        <TooltipContent>
                            <p>
                                {microphone.micError
                                    ? microphone.micError
                                    : microphone.isMicActive
                                        ? t("pages.desk.stopMic")
                                        : t("pages.desk.startMic")}
                            </p>
                        </TooltipContent>
                    </Tooltip>

                    <Popover onOpenChange={setIsMenuOpen}>
                        <PopoverTrigger asChild>
                            <Button className="controlButton" variant="ghost">
                                {isMuted || audioVolume === 0 ? <VolumeX /> : <Volume2 />}
                            </Button>
                        </PopoverTrigger>
                        <PopoverContent
                            align="center"
                            className="flex w-32 flex-col items-center gap-2 border-white/10 bg-background/90 px-3 py-4 backdrop-blur-md"
                            onOpenAutoFocus={(event) => event.preventDefault()}
                            side="top"
                            sideOffset={16}
                        >
                            <Slider
                                className="w-full"
                                max={100}
                                min={0}
                                onValueChange={(values) => onChangeVolume(values[0])}
                                step={1}
                                value={[audioVolume]}
                            />
                        </PopoverContent>
                    </Popover>

                    <div className="mx-1 h-6 w-px bg-white/20" />

                    <Tooltip>
                        <TooltipTrigger asChild>
                            <Button
                                className="controlButton text-red-500 hover:bg-red-500/20 hover:text-red-400"
                                onClick={onDisconnect}
                                variant="ghost"
                            >
                                <Power />
                            </Button>
                        </TooltipTrigger>
                        <TooltipContent>
                            <p>{t("pages.desk.disconnect")}</p>
                        </TooltipContent>
                    </Tooltip>
                </div>
            </div>
        </div>
    )
}
