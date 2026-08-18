import { useEffect, useState } from "react"
import { useForm } from "react-hook-form"
import { useTranslation } from "react-i18next"
import {
    Dialog,
    DialogContent,
    DialogHeader,
    DialogTitle,
    DialogFooter,
} from "@/components/ui/dialog"
import { Button } from "@/components/ui/button"
import {
    Form,
    FormControl,
    FormField,
    FormItem,
    FormLabel,
    FormMessage,
} from "@/components/ui/form"
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select"
import {
    Tabs,
    TabsContent,
    TabsList,
    TabsTrigger,
} from "@/components/ui/tabs"
import { Checkbox } from "@/components/ui/checkbox"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { AlertTriangle } from "lucide-react"
import type { RemoteAccessInitializedData } from "@/services/types"
import { DeskConfigAdvancedTab } from "./desk-config-advanced-tab"
import { DeskConfigAudioTab } from "./desk-config-audio-tab"
import {
    canConnectCaptureTarget,
    canEnableAdaptiveResolution,
    DESK_CONFIG_DEFAULTS,
    formatDisplayLabel,
    hasNoDisplaysForMode,
    isAudioCapabilityAvailable,
    normalizeCaptureTarget,
    orderCaptureModes,
    pickDefaultDeviceName,
    preferSavedDeskValue,
    resolveAudioCaptureMode,
    resolveAudioEncoder,
    resolveExecutableDeskConfig,
    resolveVideoEncoder,
    shouldShowAdminPrivilegeWarning,
    shouldShowNoDisplayWarning,
    toDeskDevicePreferences,
    toRemoteSessionSettings,
} from "./desk-config-model"
import type { DeskConfigFormSettings, DeskConfigSubmission } from "./desk-config-model"
import {
    DEFAULT_DESK_DEVICE_PREFERENCES,
    type DeskDevicePreferencesV1,
} from "./desk-preferences"

interface DeskConfigDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
    initData: RemoteAccessInitializedData | null
    preferences: DeskDevicePreferencesV1 | null
    onSubmit: (
        settings: DeskConfigSubmission,
        preferences: DeskDevicePreferencesV1,
    ) => void
    onCancel: () => void
    /**
     * Browser-side adaptive video quality toggle. Owned by the parent
     * (persisted in localStorage there) and surfaced in this dialog so
     * the user can disable the packet-loss / RTT driven encoder
     * rebuild loop. It is a user-level browser preference; live
     * samples cross the signaling boundary only through the narrow
     * `UpdateAdaptiveVideoQuality` command.
     */
    adaptiveQualityEnabled: boolean
    onAdaptiveQualityChange: (enabled: boolean) => void
    /**
     * Server-side adaptive bitrate-cap toggle (the REMB-driven inner
     * loop that trims encoder bitrate spikes without touching the
     * quality knob). Owned by the parent (persisted in localStorage
     * there); the parent injects it into `RemoteSessionSettings` on
     * connect / apply, where the daemon applies it to this connection only.
     */
    adaptiveBitrateEnabled: boolean
    onAdaptiveBitrateChange: (enabled: boolean) => void
    systemAudioAllowed: boolean
}

export function DeskConfigDialog({
    open,
    onOpenChange,
    initData,
    preferences,
    onSubmit,
    onCancel,
    adaptiveQualityEnabled,
    onAdaptiveQualityChange,
    adaptiveBitrateEnabled,
    onAdaptiveBitrateChange,
    systemAudioAllowed,
}: DeskConfigDialogProps) {
    const { t } = useTranslation()

    const form = useForm<DeskConfigFormSettings>({
        defaultValues: DESK_CONFIG_DEFAULTS,
    })

    /**
     * Set to the previously-saved `video_device_name` when the user
     * opens the dialog with a configuration that points at a display
     * that no longer exists (display unplugged between sessions, or
     * the IDD virtual monitor was detached). Surfaces as a warning
     * banner above the dropdown; cleared once the user picks a
     * different display.
     */
    const [staleSavedDeviceName, setStaleSavedDeviceName] = useState<string | null>(null)
    const [staleSavedCaptureMode, setStaleSavedCaptureMode] = useState<string | null>(null)

    // Auto-resolution semantically targets the IDD; if the captured
    // display is not the IDD, the request would silently change the
    // IDD resolution while WGC keeps capturing a physical screen.
    // Watch the selected device and force-uncheck the adaptive toggle
    // whenever the selection drifts off the IDD, so the user can never
    // submit a misconfiguration. The toggle is also visually disabled
    // by the render path; this effect handles the data side.
    const watchedDeviceName = form.watch("video_device_name")
    const watchedAdaptive = form.watch("adaptive_web_page_resolution")
    const virtualDisplayName = initData?.virtual_display_device_name ?? null
    useEffect(() => {
        const canEnable = canEnableAdaptiveResolution(
            watchedDeviceName,
            virtualDisplayName,
        )
        if (!canEnable && watchedAdaptive) {
            form.setValue("adaptive_web_page_resolution", false)
        }
    }, [watchedDeviceName, watchedAdaptive, virtualDisplayName, form])

    useEffect(() => {
        if (!initData?.suggested_session_settings) {
            return
        }
        const suggested = initData.suggested_session_settings
        const capabilities = initData.session_settings_capabilities
        const hasSavedPreferences = preferences !== null
        const intent = preferences ?? DEFAULT_DESK_DEVICE_PREFERENCES
        const target = normalizeCaptureTarget(
            capabilities.image_capture === "unsupported"
                ? suggested.image_capture
                : intent.imageCapture ?? suggested.image_capture,
            capabilities.video_device_name === "unsupported"
                ? suggested.video_device_name
                : intent.videoDeviceName ?? suggested.video_device_name,
            initData.video_device_list,
        )
        const audioCapture = resolveAudioCaptureMode(
            capabilities.audio_capture === "unsupported"
                ? suggested.audio_capture
                : intent.audioCapture,
            suggested.audio_capture,
            initData.audio_device_list,
        )
        const audioDevices = audioCapture
            ? initData.audio_device_list?.[audioCapture] ?? []
            : []
        const explicitAudioDevice = capabilities.audio_device !== "unsupported"
            && intent.audioDevice
            && audioDevices.some((device) => (
                device.id === intent.audioDevice?.audioDeviceId
                && device.data_flow === intent.audioDevice.audioDataFlow
            ))
            ? {
                audio_data_flow: intent.audioDevice.audioDataFlow,
                audio_device_id: intent.audioDevice.audioDeviceId,
            }
            : null
        const suggestedAudioDevice = suggested.audio_device
            && audioDevices.some((device) => (
                device.data_flow === suggested.audio_device?.audio_data_flow
                && (suggested.audio_device.audio_device_id === null
                    || device.id === suggested.audio_device.audio_device_id)
            ))
            ? suggested.audio_device
            : null
        const defaultAudioDevice = audioDevices.find((device) => device.default)
            ?? audioDevices[0]
        const audioDevice = explicitAudioDevice ?? suggestedAudioDevice ?? (defaultAudioDevice
            ? {
                audio_data_flow: defaultAudioDevice.data_flow,
                audio_device_id: null,
            }
            : null)
        const audioCapabilityAvailable = isAudioCapabilityAvailable(
            initData,
            systemAudioAllowed,
        )
        setStaleSavedCaptureMode(target.staleMode)
        setStaleSavedDeviceName(target.staleDevice)
        form.reset({
            ...suggested,
            audio_capture: audioCapture,
            audio_device: audioDevice,
            audio_encoder: resolveAudioEncoder(
                hasSavedPreferences ? intent.audioEncoder : null,
                suggested.audio_encoder,
                initData.audio_encoder_list,
            ),
            enable_audio: audioCapabilityAvailable && (
                hasSavedPreferences ? intent.captureAudio : suggested.capture_audio
            ),
            image_capture: target.effectiveMode,
            video_device_name: target.effectiveDeviceName,
            show_mouse: capabilities.show_mouse === "unsupported"
                ? suggested.show_mouse
                : preferSavedDeskValue(preferences, intent.showMouse, suggested.show_mouse),
            adaptive_web_page_resolution: intent.adaptiveWebPageResolution,
            video_encoder: resolveVideoEncoder(
                hasSavedPreferences ? intent.videoEncoder : null,
                suggested.video_encoder,
                initData.video_encoder_list,
            ),
            video_fps: capabilities.video_fps === "unsupported"
                ? suggested.video_fps
                : preferSavedDeskValue(
                    preferences,
                    intent.videoFps ?? suggested.video_fps,
                    suggested.video_fps,
                ),
            video_quality: capabilities.video_quality === "unsupported"
                ? suggested.video_quality
                : preferSavedDeskValue(
                    preferences,
                    intent.videoQuality,
                    suggested.video_quality,
                ),
            wayland_control_mode: intent.waylandControlMode,
            enable_dirty_rect: capabilities.enable_dirty_rect === "unsupported"
                ? suggested.enable_dirty_rect
                : preferSavedDeskValue(
                    preferences,
                    intent.enableDirtyRect,
                    suggested.enable_dirty_rect,
                ),
        })
    }, [initData, preferences, form, systemAudioAllowed])

    const rawCaptureKeys = initData && initData.video_device_list ? Object.keys(initData.video_device_list) : []
    const imageCaptureList = orderCaptureModes(rawCaptureKeys)
    const selectedImageCapture = form.watch("image_capture")
    const videoDeviceList = initData && selectedImageCapture && initData.video_device_list
        ? initData.video_device_list[selectedImageCapture]
        : []
    const noDisplaysForMode = hasNoDisplaysForMode(selectedImageCapture, videoDeviceList)
    const captureTarget = normalizeCaptureTarget(
        selectedImageCapture,
        watchedDeviceName,
        initData?.video_device_list,
    )
    const canConnect = !!initData && canConnectCaptureTarget(
        selectedImageCapture,
        watchedDeviceName,
        initData.video_device_list,
    )
    const captureUnavailable = !!initData && !captureTarget.hasUsableCaptureTarget
    const showNoDisplaysForMode = shouldShowNoDisplayWarning(
        captureUnavailable,
        noDisplaysForMode,
    )

    const handleSubmit = (values: DeskConfigFormSettings) => {
        if (!initData) return
        const executableValues = resolveExecutableDeskConfig(values, initData)
        // Defence in depth: never start a session without a display, even if the
        // disabled submit button is bypassed (e.g. implicit Enter-key submit).
        if (!canConnectCaptureTarget(
            executableValues.image_capture,
            executableValues.video_device_name,
            initData.video_device_list,
        )) {
            return
        }
        onSubmit(
            toRemoteSessionSettings(
                executableValues,
                initData.session_settings_capabilities.adaptive_bitrate === "unsupported"
                    ? initData.suggested_session_settings.adaptive_bitrate
                    : adaptiveBitrateEnabled,
            ),
            toDeskDevicePreferences(values),
        )
    }

    const handleInteractOutside = (e: Event) => {
        e.preventDefault() // Prevent closing on outside click
    }

    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent className="sm:max-w-[425px] max-h-[90vh] overflow-y-auto" onInteractOutside={handleInteractOutside}>
                <DialogHeader>
                    <DialogTitle>{t('pages.desk.deskConfig')}</DialogTitle>
                </DialogHeader>
                {shouldShowAdminPrivilegeWarning(
                    initData?.is_admin,
                    initData?.operation_system,
                ) && (
                    <Alert variant="destructive">
                        <AlertTriangle className="h-4 w-4" />
                        <AlertTitle>{t('pages.system.settings.alert.message')}</AlertTitle>
                        <AlertDescription>
                            {t('pages.desk.adminPrivilegeWarning')}
                        </AlertDescription>
                    </Alert>
                )}
                <Form {...form}>
                    <form onSubmit={form.handleSubmit(handleSubmit)} className="space-y-4">
                        <Tabs defaultValue="display" className="w-full">
                            <TabsList className="grid w-full grid-cols-3">
                                <TabsTrigger value="display">{t('pages.desk.display')}</TabsTrigger>
                                <TabsTrigger value="audio">{t('pages.desk.audio')}</TabsTrigger>
                                <TabsTrigger value="advanced">{t('pages.desk.advanced')}</TabsTrigger>
                            </TabsList>

                            {/* --- DISPLAY TAB --- */}
                            <TabsContent value="display" className="space-y-4 pt-4">
                                <FormField
                                    control={form.control}
                                    name="image_capture"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.screenCaptureMode')}</FormLabel>
                                            {/* Radix Select + RHF reset can miss value updates; re-mount to reflect initial data */}
                                            {(() => {
                                                const currentValue = field.value || ""
                                                return (
                                            <Select
                                                disabled={initData?.session_settings_capabilities.image_capture === "unsupported"}
                                                key={`image-capture-${currentValue || "empty"}`}
                                                onValueChange={(value: string) => {
                                                    field.onChange(value)
                                                    // Reset device selection to the new
                                                    // backend's primary monitor: the saved
                                                    // device_name is only meaningful for the
                                                    // backend that enumerated it (DXGI never
                                                    // sees IDD, etc.). The user can re-pick
                                                    // from the dropdown below.
                                                    const next = initData?.video_device_list
                                                        ? initData.video_device_list[value] ?? []
                                                        : []
                                                    form.setValue(
                                                        "video_device_name",
                                                        pickDefaultDeviceName(next),
                                                    )
                                                    setStaleSavedDeviceName(null)
                                                    setStaleSavedCaptureMode(null)
                                                }}
                                                defaultValue={currentValue}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder={t('pages.desk.screenCaptureModePlaceholder')} />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    {imageCaptureList.map((mode) => (
                                                        <SelectItem key={mode} value={mode}>
                                                            {mode}
                                                        </SelectItem>
                                                    ))}
                                                </SelectContent>
                                            </Select>
                                                )
                                            })()}
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                {selectedImageCapture === "DXGI" && (
                                    <Alert>
                                        <AlertTriangle className="h-4 w-4" />
                                        <AlertTitle>
                                            {t("pages.desk.dxgiVideoBlackBarWarning.title")}
                                        </AlertTitle>
                                        <AlertDescription>
                                            {t(
                                                "pages.desk.dxgiVideoBlackBarWarning.body"
                                            )}
                                        </AlertDescription>
                                    </Alert>
                                )}

                                {captureUnavailable && (
                                    <Alert variant="destructive">
                                        <AlertTriangle className="h-4 w-4" />
                                        <AlertTitle>{t("pages.desk.captureUnavailable.title")}</AlertTitle>
                                        <AlertDescription>
                                            {t("pages.desk.captureUnavailable.body")}
                                        </AlertDescription>
                                    </Alert>
                                )}

                                {staleSavedCaptureMode && (
                                    <Alert>
                                        <AlertTriangle className="h-4 w-4" />
                                        <AlertTitle>{t("pages.desk.captureModeStaleWarning.title")}</AlertTitle>
                                        <AlertDescription>
                                            {t("pages.desk.captureModeStaleWarning.body", {
                                                name: staleSavedCaptureMode,
                                            })}
                                        </AlertDescription>
                                    </Alert>
                                )}

                                {showNoDisplaysForMode && (
                                    <Alert variant="destructive">
                                        <AlertTriangle className="h-4 w-4" />
                                        <AlertTitle>
                                            {t(
                                                "pages.desk.noDisplayWarning.title",
                                            )}
                                        </AlertTitle>
                                        <AlertDescription>
                                            {t(
                                                "pages.desk.noDisplayWarning.body",
                                            )}
                                        </AlertDescription>
                                    </Alert>
                                )}

                                {selectedImageCapture && videoDeviceList && videoDeviceList.length > 0 && (
                                    <>
                                        {staleSavedDeviceName && (
                                            <Alert>
                                                <AlertTriangle className="h-4 w-4" />
                                                <AlertTitle>
                                                    {t(
                                                        "pages.desk.displayDeviceStaleWarning.title",
                                                    )}
                                                </AlertTitle>
                                                <AlertDescription>
                                                    {t(
                                                        "pages.desk.displayDeviceStaleWarning.body",
                                                        { name: staleSavedDeviceName },
                                                    )}
                                                </AlertDescription>
                                            </Alert>
                                        )}
                                        {/* Hint surfaced only when the daemon reports an
                                            attached IDD that also appears in the current
                                            backend's enumeration. Without the
                                            `.some(...)` check, switching to a non-IDD-aware
                                            backend (e.g. legacy DXGI builds) would still
                                            show the hint even though no entry in the
                                            dropdown can satisfy the adaptive toggle. */}
                                        {initData?.virtual_display_device_name &&
                                            videoDeviceList.some(
                                                (d) => d.device_name === initData.virtual_display_device_name,
                                            ) && (
                                                <p className="text-xs text-muted-foreground">
                                                    {t(
                                                        "pages.desk.virtualDisplayHint",
                                                    )}
                                                </p>
                                            )}
                                        <FormField
                                            control={form.control}
                                            name="video_device_name"
                                            rules={{
                                                required: t(
                                                    "pages.desk.displayDeviceRequired",
                                                ),
                                                validate: (value: string | undefined) =>
                                                    (value !== undefined && value !== "") ||
                                                    t(
                                                        "pages.desk.displayDeviceRequired",
                                                    ),
                                            }}
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t('pages.desk.displayDevice')}</FormLabel>
                                                    {(() => {
                                                        const currentValue = field.value ?? ""
                                                        return (
                                                    <Select
                                                        disabled={initData?.session_settings_capabilities.video_device_name === "unsupported"}
                                                        onValueChange={(value: string) => {
                                                            field.onChange(value)
                                                            setStaleSavedDeviceName(null)
                                                        }}
                                                        key={`video-device-${currentValue || "empty"}`}
                                                        defaultValue={currentValue}
                                                    >
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder={t('pages.desk.displayDevicePlaceholder')} />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            {videoDeviceList.map((device) => {
                                                                // `textValue` keeps Radix's typeahead and
                                                                // the SelectValue display text equal to the
                                                                // plain label — otherwise the badge `<span>`
                                                                // children would leak the word "Virtual"
                                                                // into the selected-value rendering.
                                                                const label = formatDisplayLabel(device)
                                                                const isVirtual =
                                                                    !!initData?.virtual_display_device_name &&
                                                                    device.device_name ===
                                                                        initData.virtual_display_device_name
                                                                return (
                                                                    <SelectItem
                                                                        key={device.device_name}
                                                                        value={device.device_name ?? ""}
                                                                        textValue={label}
                                                                    >
                                                                        {isVirtual && (
                                                                            <span className="mr-2 inline-flex items-center rounded-md bg-blue-500/15 px-1.5 py-0.5 text-xs font-medium text-blue-500">
                                                                                {t(
                                                                                    "pages.desk.virtualDisplayBadge",
                                                                                )}
                                                                            </span>
                                                                        )}
                                                                        {label}
                                                                    </SelectItem>
                                                                )
                                                            })}
                                                        </SelectContent>
                                                    </Select>
                                                        )
                                                    })()}
                                                    <FormMessage />
                                                </FormItem>
                                            )}
                                        />
                                    </>
                                )}

                                <FormField
                                    control={form.control}
                                    name="show_mouse"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-start space-x-3 space-y-0 p-2 rounded-md border">
                                            <FormControl>
                                                <Checkbox
                                                    checked={field.value}
                                                    disabled={initData?.session_settings_capabilities.show_mouse === "unsupported"}
                                                    onCheckedChange={field.onChange}
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel>
                                                    {t('pages.desk.showRemoteMouse')}
                                                </FormLabel>
                                            </div>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="adaptive_web_page_resolution"
                                    render={({ field }) => {
                                        // `canEnable` mirrors the effect above —
                                        // re-derived per render against the same inputs
                                        // so the visual state stays consistent.
                                        const canEnable = canEnableAdaptiveResolution(
                                            watchedDeviceName,
                                            virtualDisplayName,
                                        )
                                        return (
                                            <FormItem className="flex flex-row items-start space-x-3 space-y-0 p-2 rounded-md border">
                                                <FormControl>
                                                    <Checkbox
                                                        checked={!!field.value}
                                                        onCheckedChange={field.onChange}
                                                        disabled={!canEnable}
                                                    />
                                                </FormControl>
                                                <div className="space-y-1 leading-none">
                                                    <FormLabel>
                                                        {t('pages.desk.adaptiveResolution')}
                                                    </FormLabel>
                                                    {!canEnable && (
                                                        <p className="text-xs text-muted-foreground">
                                                            {t(
                                                                'pages.desk.adaptiveResolutionVirtualOnly',
                                                            )}
                                                        </p>
                                                    )}
                                                </div>
                                            </FormItem>
                                        )
                                    }}
                                />

                            </TabsContent>

                            {/* --- AUDIO TAB --- */}
                            <DeskConfigAudioTab
                                form={form}
                                initData={initData}
                                systemAudioAllowed={systemAudioAllowed}
                            />

                            {/* --- ADVANCED TAB --- */}
                            <DeskConfigAdvancedTab
                                adaptiveBitrateEnabled={adaptiveBitrateEnabled}
                                adaptiveQualityEnabled={adaptiveQualityEnabled}
                                form={form}
                                initData={initData}
                                onAdaptiveBitrateChange={onAdaptiveBitrateChange}
                                onAdaptiveQualityChange={onAdaptiveQualityChange}
                            />
                        </Tabs>

                        <DialogFooter className="pt-4">
                            <Button type="button" variant="outline" onClick={onCancel}>
                                {t('pages.desk.close')}
                            </Button>
                            <Button type="submit" disabled={!canConnect}>
                                {t('pages.desk.connect')}
                            </Button>
                        </DialogFooter>
                    </form>
                </Form>
            </DialogContent>
        </Dialog>
    )
}
