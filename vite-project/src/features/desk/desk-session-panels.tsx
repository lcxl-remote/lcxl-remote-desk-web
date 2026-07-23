import type { ReactNode } from "react"
import { useTranslation } from "react-i18next"
import {
    AlertCircle,
    CheckCircle2,
    Loader2,
    SignalHigh,
    SignalLow,
    SignalMedium,
    XSquare,
} from "lucide-react"

import { Button } from "@/components/ui/button"
import { connectionQuality } from "./connection-quality"
import type { RTCStatsData } from "./use-desk-rtc"
import type { ResolutionToast } from "./use-resolution-toast"

export function ConnectionQualityBadge({
    packetLoss,
    rtt,
}: {
    packetLoss: number
    rtt: number
}) {
    const { t } = useTranslation()
    const quality = connectionQuality(packetLoss, rtt)
    const config = {
        good: { icon: SignalHigh, className: "text-green-500", label: t("pages.desk.quality.good") },
        fair: { icon: SignalMedium, className: "text-amber-500", label: t("pages.desk.quality.fair") },
        poor: { icon: SignalLow, className: "text-red-500", label: t("pages.desk.quality.poor") },
    }[quality]
    const Icon = config.icon

    return (
        <span
            className={`flex items-center gap-1 text-sm ${config.className}`}
            title={t("pages.desk.quality.detail", {
                loss: packetLoss,
                rtt: Math.round(rtt),
            })}
        >
            <Icon className="h-4 w-4" />
            <span className="hidden sm:inline">{config.label}</span>
        </span>
    )
}

type DeskSessionStatsProps = {
    adaptiveQualityEnabled: boolean
    currentVideoQuality: number | null
    lastQualityAdjustedAt: number
    onClose: () => void
    qualityAdjustmentCount: number
    rtcStats: RTCStatsData
}

function StatSection({
    children,
    title,
}: {
    children: ReactNode
    title: string
}) {
    return (
        <>
            <div className="mt-3 mb-1 border-t border-white/15 pt-2 text-xs font-bold text-white/80">
                {title}
            </div>
            {children}
        </>
    )
}

function StatRow({
    children,
    label,
}: {
    children: ReactNode
    label: string
}) {
    return (
        <div className="mb-1 flex justify-between gap-4">
            <span className="text-gray-400">{label}:</span>
            {children}
        </div>
    )
}

export function DeskSessionStats({
    adaptiveQualityEnabled,
    currentVideoQuality,
    lastQualityAdjustedAt,
    onClose,
    qualityAdjustmentCount,
    rtcStats,
}: DeskSessionStatsProps) {
    const { t } = useTranslation()
    const qualityClass = (currentVideoQuality ?? 22) > 40
        ? "text-red-400"
        : (currentVideoQuality ?? 22) > 30
            ? "text-yellow-300"
            : "text-green-400"

    return (
        <div className="absolute top-4 left-4 z-50 max-h-[80vh] min-w-[260px] select-none overflow-y-auto rounded-lg border border-white/20 bg-black/60 p-3 font-mono text-xs text-white backdrop-blur-md">
            <div className="mb-2 flex items-center justify-between border-b border-white/15 pb-1">
                <div className="text-sm font-bold text-white/90">
                    {t("pages.desk.statsPanel.title")}
                </div>
                <button
                    aria-label={t("pages.desk.closeStats")}
                    className="text-gray-400 transition-colors hover:text-white"
                    onClick={onClose}
                >
                    <XSquare className="h-4 w-4" />
                </button>
            </div>

            <StatRow label={t("pages.desk.statsPanel.fps")}>
                <span className="font-bold text-green-400">{rtcStats.fps}</span>
            </StatRow>
            <StatRow label={t("pages.desk.statsPanel.resolution")}>
                <span className="font-bold text-white">{rtcStats.width}x{rtcStats.height}</span>
            </StatRow>
            <StatRow label={t("pages.desk.statsPanel.bitrate")}>
                <span className="font-bold text-blue-400">{rtcStats.bitrate} kbps</span>
            </StatRow>
            <StatRow label={t("pages.desk.statsPanel.videoCodec")}>
                <span className="font-bold text-purple-400">{rtcStats.videoCodec || "Unknown"}</span>
            </StatRow>
            {rtcStats.audioCodec && (
                <StatRow label={t("pages.desk.statsPanel.audioCodec")}>
                    <span className="font-bold text-purple-400">{rtcStats.audioCodec}</span>
                </StatRow>
            )}
            <StatRow label={t("pages.desk.statsPanel.latency")}>
                <span className={`font-bold ${rtcStats.rtt > 150 ? "text-red-400" : rtcStats.rtt > 80 ? "text-yellow-400" : "text-green-400"}`}>
                    {rtcStats.rtt} ms
                </span>
            </StatRow>
            <StatRow label={t("pages.desk.statsPanel.packetLoss")}>
                <span className={`font-bold ${rtcStats.packetLoss > 5 ? "text-red-400" : rtcStats.packetLoss > 1 ? "text-yellow-400" : "text-green-400"}`}>
                    {rtcStats.packetLoss}%
                </span>
            </StatRow>
            <StatRow label={t("pages.desk.statsPanel.network")}>
                <span className="font-bold uppercase text-orange-400">
                    {rtcStats.networkType || "Unknown"}
                </span>
            </StatRow>

            <StatSection title={t("pages.desk.statsPanel.frameSection")}>
                <StatRow label={t("pages.desk.statsPanel.framesDecoded")}>
                    <span className="font-bold text-white">
                        {rtcStats.framesDecoded} (+{rtcStats.framesDecodedDelta})
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.keyFrames")}>
                    <span className={`font-bold ${rtcStats.keyFramesDelta > 1 ? "text-red-400" : "text-yellow-300"}`}>
                        {rtcStats.keyFramesDecoded} (+{rtcStats.keyFramesDelta})
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.pFrames")}>
                    <span className="font-bold text-white">{rtcStats.pFramesDecoded}</span>
                </StatRow>
                {rtcStats.videoCodec === "VP9" && (
                    <div className="mt-0.5 text-[10px] leading-tight text-gray-500 italic">
                        {t("pages.desk.statsPanel.vp9FrameTypeHint")}
                    </div>
                )}
                <StatRow label={t("pages.desk.statsPanel.framesDropped")}>
                    <span className={`font-bold ${rtcStats.framesDropped > 0 ? "text-yellow-300" : "text-white"}`}>
                        {rtcStats.framesDropped}
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.avgQp")}>
                    <span className={`font-bold ${rtcStats.avgQp === null ? "text-gray-500 italic" : "text-white"}`}>
                        {rtcStats.avgQp === null
                            ? t("pages.desk.statsPanel.avgQpUnavailable")
                            : rtcStats.avgQp}
                    </span>
                </StatRow>
            </StatSection>

            <StatSection title={t("pages.desk.statsPanel.feedbackSection")}>
                <StatRow label={t("pages.desk.statsPanel.pliCount")}>
                    <span className={`font-bold ${rtcStats.pliDelta > 0 ? "text-red-400" : "text-white"}`}>
                        {rtcStats.pliCount} (+{rtcStats.pliDelta})
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.nackCount")}>
                    <span className="font-bold text-white">{rtcStats.nackCount}</span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.firCount")}>
                    <span className="font-bold text-white">{rtcStats.firCount}</span>
                </StatRow>
            </StatSection>

            <StatSection title={t("pages.desk.statsPanel.encoderSection")}>
                <StatRow label={t("pages.desk.statsPanel.currentQuality")}>
                    <span className={`font-bold ${qualityClass}`}>
                        {currentVideoQuality ?? "-"}
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.qualityAdjustments")}>
                    <span className={`font-bold ${adaptiveQualityEnabled
                        ? qualityAdjustmentCount > 5 ? "text-yellow-300" : "text-white"
                        : "text-gray-500"}`}
                    >
                        {qualityAdjustmentCount}
                        {!adaptiveQualityEnabled && " (off)"}
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.lastAdjustedAgo")}>
                    <span className="font-bold text-white">
                        {qualityAdjustmentCount === 0
                            ? "-"
                            : `${Math.max(0, Math.round((Date.now() - lastQualityAdjustedAt) / 1000))} s`}
                    </span>
                </StatRow>
            </StatSection>

            <StatSection title={t("pages.desk.statsPanel.qualitySection")}>
                <StatRow label={t("pages.desk.statsPanel.freezeCount")}>
                    <span className={`font-bold ${rtcStats.freezeCount > 0 ? "text-yellow-300" : "text-white"}`}>
                        {rtcStats.freezeCount}
                    </span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.totalFreezeDuration")}>
                    <span className="font-bold text-white">{rtcStats.totalFreezesDurationMs} ms</span>
                </StatRow>
                <StatRow label={t("pages.desk.statsPanel.jitter")}>
                    <span className={`font-bold ${rtcStats.jitterMs > 50 ? "text-yellow-300" : "text-white"}`}>
                        {rtcStats.jitterMs} ms
                    </span>
                </StatRow>
                <div className="mt-0.5 text-[10px] leading-tight text-gray-500 italic">
                    {t("pages.desk.statsPanel.jitterHint")}
                </div>
            </StatSection>
        </div>
    )
}

export function ClipboardFallbackToast({
    onClose,
    onSync,
    text,
}: {
    onClose: () => void
    onSync: () => void
    text?: string
}) {
    return (
        <div className="pointer-events-auto absolute right-4 bottom-24 z-[60] flex min-w-[300px] flex-col gap-3 rounded-lg bg-amber-500/90 p-4 text-white shadow-xl animate-in slide-in-from-bottom-4">
            <div className="flex items-start justify-between">
                <span className="text-sm font-semibold">Action Required</span>
                <button className="text-white/80 hover:text-white" onClick={onClose}>
                    <XSquare className="h-4 w-4" />
                </button>
            </div>
            <p className="text-xs text-amber-100">
                {text || "Clipboard update received, please click to sync."}
            </p>
            <Button
                className="w-full bg-white text-amber-900 hover:bg-amber-100"
                onClick={onSync}
                size="sm"
                variant="secondary"
            >
                Sync Now
            </Button>
        </div>
    )
}

export function ResolutionStatusToast({
    toast,
}: {
    toast: Exclude<ResolutionToast, null>
}) {
    const { t } = useTranslation()

    return (
        <div
            className={`pointer-events-none absolute right-4 bottom-40 z-[60] flex items-center gap-2 rounded-lg border px-3 py-2 text-xs font-medium shadow-lg backdrop-blur-md animate-in slide-in-from-bottom-4 ${
                toast.phase === "success"
                    ? "border-emerald-300/40 bg-emerald-500/90 text-white"
                    : toast.phase === "failed"
                        ? "border-red-300/40 bg-red-500/90 text-white"
                        : "border-white/10 bg-black/80 text-white"
            }`}
            data-phase={toast.phase}
            data-testid="resolution-toast"
        >
            {toast.phase === "updating" && (
                <>
                    <Loader2 className="h-4 w-4 animate-spin text-blue-300" />
                    <span>
                        {t("pages.desk.resolutionUpdating", {
                            w: toast.targetW,
                            h: toast.targetH,
                        })}
                    </span>
                </>
            )}
            {toast.phase === "success" && (
                <>
                    <CheckCircle2 className="h-4 w-4" />
                    <span>
                        {t("pages.desk.resolutionApplied", {
                            w: toast.appliedW,
                            h: toast.appliedH,
                        })}
                    </span>
                </>
            )}
            {toast.phase === "failed" && (
                <>
                    <AlertCircle className="h-4 w-4" />
                    <span>
                        {t("pages.desk.resolutionFailed", {
                            reason: toast.reason,
                        })}
                    </span>
                </>
            )}
        </div>
    )
}
