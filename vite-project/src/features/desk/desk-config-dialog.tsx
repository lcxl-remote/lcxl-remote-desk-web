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
import { Label } from "@/components/ui/label"
import { Slider } from "@/components/ui/slider"
import { Input } from "@/components/ui/input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { AlertTriangle } from "lucide-react"
import type { InitSignalingData, DeskSettings, DisplayInfo } from "@/services/types"

/**
 * Compose the human-readable label shown for a `DisplayInfo` in the
 * display-picker dropdown.
 *
 * The size suffix has to be computed as `right - left` and
 * `bottom - top`: `desktop_coordinates` is a rectangle in virtual
 * desktop space, not a `(width, height)` pair. Off-origin monitors
 * (an IDD attached to the right of the primary, or any second
 * monitor) have `left > 0`, so reading `right` / `bottom` directly
 * adds the offset into the displayed resolution. Exported so the
 * unit test can guard against this regression without touching the
 * Radix Select internals.
 */
export function formatDisplayLabel(device: DisplayInfo): string {
    const name = device.display_device_name ?? device.device_name
    const r = device.desktop_coordinates
    const width = r.right - r.left
    const height = r.bottom - r.top
    return `${name} (${width}x${height})`
}

interface DeskConfigDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
    initData: InitSignalingData | null
    onSubmit: (settings: DeskSettings) => void
    onCancel: () => void
    /**
     * Browser-side adaptive video quality toggle. Owned by the parent
     * (persisted in localStorage there) and surfaced in this dialog so
     * the user can disable the packet-loss / RTT driven encoder
     * rebuild loop. Not part of `DeskSettings` because it never
     * crosses the signaling boundary — it only gates whether the
     * browser sends `UpdateDeskSettings(video_quality=...)` from the
     * stats observer.
     */
    adaptiveQualityEnabled: boolean
    onAdaptiveQualityChange: (enabled: boolean) => void
}

export function DeskConfigDialog({
    open,
    onOpenChange,
    initData,
    onSubmit,
    onCancel,
    adaptiveQualityEnabled,
    onAdaptiveQualityChange,
}: DeskConfigDialogProps) {
    const { t } = useTranslation()

    // Extend DeskSettings for form state
    type FormSettings = DeskSettings & { enable_audio: boolean }

    const form = useForm<FormSettings>({
        defaultValues: {
            image_capture: "",
            video_device_name: "",
            show_mouse: true,
            adaptive_web_page_resolution: true,
            video_zoom_ratio: 100,
            video_quality: 22,
            enable_audio: false,
            video_encoder: null,
            audio_encoder: null,
            wayland_control_mode: "auto",
            video_fps: undefined,
            enable_dirty_rect: true,
        },
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

    /**
     * Pick a default `device_name` from a backend's
     * `video_device_list`. Prefers the primary monitor when the
     * backend surfaces one (DisplayInfo whose `desktop_coordinates`
     * include the origin point 0,0 — the standard GDI primary-monitor
     * convention), otherwise falls back to the first entry. Returns
     * the empty string when the list is empty so the form gates submit
     * (an empty list is the headless / capture-unsupported state).
     */
    const pickDefaultDeviceName = (list: ReadonlyArray<DisplayInfo>): string => {
        if (list.length === 0) {
            return ""
        }
        const primary = list.find((d) => {
            const r = d.desktop_coordinates
            return r && r.left === 0 && r.top === 0
        })
        return (primary ?? list[0]).device_name ?? ""
    }

    useEffect(() => {
        if (!initData?.desk_settings) {
            return
        }
        const saved = initData.desk_settings
        const backend = saved.image_capture ?? ""
        const candidates: ReadonlyArray<DisplayInfo> = (backend &&
            initData.video_device_list &&
            initData.video_device_list[backend]) || []
        const savedName = saved.video_device_name ?? ""
        let resolvedName = savedName
        let stale: string | null = null
        if (savedName === "") {
            resolvedName = pickDefaultDeviceName(candidates)
        } else if (!candidates.some((d) => d.device_name === savedName)) {
            // Saved display is gone (hot-plug / IDD detached / config
            // edited by hand). Prefill the primary so the user can hit
            // Connect without re-picking, but surface a warning so they
            // know we didn't honour their persisted choice.
            stale = savedName
            resolvedName = pickDefaultDeviceName(candidates)
        }
        setStaleSavedDeviceName(stale)
        form.reset({
            ...saved,
            video_device_name: resolvedName,
            show_mouse: saved.show_mouse ?? true,
            adaptive_web_page_resolution: saved.adaptive_web_page_resolution ?? true,
            video_zoom_ratio: saved.video_zoom_ratio ?? 100,
            video_quality: saved.video_quality ?? 22,
            wayland_control_mode: saved.wayland_control_mode ?? "auto",
            enable_dirty_rect: saved.enable_dirty_rect ?? true,
        })
    }, [initData, form])

    // Backend returns `video_device_list` as a JSON-serialized
    // BTreeMap, which sorts alphabetically (D < G < W). Pin a preferred
    // order so WGC — the only backend that correctly captures hardware
    // overlay surfaces like browser-decoded video — appears first.
    const CAPTURE_PREFERRED_ORDER = ["WGC", "DXGI", "GDI"] as const
    const rawCaptureKeys = initData && initData.video_device_list ? Object.keys(initData.video_device_list) : []
    const imageCaptureList = [
        ...CAPTURE_PREFERRED_ORDER.filter((k) => rawCaptureKeys.includes(k)),
        ...rawCaptureKeys
            .filter((k) => !(CAPTURE_PREFERRED_ORDER as readonly string[]).includes(k))
            .sort(),
    ]
    const selectedImageCapture = form.watch("image_capture")
    const videoDeviceList = initData && selectedImageCapture && initData.video_device_list
        ? initData.video_device_list[selectedImageCapture]
        : []

    // Audio states
    const enableAudio = form.watch("enable_audio")
    const audioCaptureList = initData && initData.audio_device_list ? Object.keys(initData.audio_device_list) : []
    const selectedAudioCapture = form.watch("audio_capture")
    const audioDeviceList = initData && selectedAudioCapture && initData.audio_device_list
        ? initData.audio_device_list[selectedAudioCapture]
        : []

    const handleSubmit = (values: FormSettings) => {
        // Ensure numbers are properly typed
        const submitData: DeskSettings = {
            ...values,
            video_device_name: values.video_device_name ?? "",
            video_zoom_ratio: Number(values.video_zoom_ratio),
            video_quality: Number(values.video_quality),
            wayland_control_mode: values.wayland_control_mode ?? "auto",
        }

        if (values.video_fps !== undefined && values.video_fps !== null) {
            const fps = Number(values.video_fps);
            submitData.video_fps = isNaN(fps) || fps <= 0 ? undefined : fps;
        } else {
            submitData.video_fps = undefined;
        }

        // Strip out non-schema data
        if (!values.enable_audio) {
            submitData.audio_device = null
        }
        delete (submitData as any).enable_audio;

        onSubmit(submitData)
    }

    const handleInteractOutside = (e: Event) => {
        e.preventDefault() // Prevent closing on outside click
    }

    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent className="sm:max-w-[425px] max-h-[90vh] overflow-y-auto" onInteractOutside={handleInteractOutside}>
                <DialogHeader>
                    <DialogTitle>{t('pages.desk.deskConfig', 'Remote Desk Configuration')}</DialogTitle>
                </DialogHeader>
                {initData?.is_admin === false && (
                    <Alert variant="destructive">
                        <AlertTriangle className="h-4 w-4" />
                        <AlertTitle>{t('pages.system.settings.alert.message', 'Warning')}</AlertTitle>
                        <AlertDescription>
                            {t('pages.desk.adminPrivilegeWarning', 'The remote server is not running with administrative/root privileges. Some operations may be restricted.')}
                        </AlertDescription>
                    </Alert>
                )}
                <Form {...form}>
                    <form onSubmit={form.handleSubmit(handleSubmit)} className="space-y-4">
                        <Tabs defaultValue="display" className="w-full">
                            <TabsList className="grid w-full grid-cols-3">
                                <TabsTrigger value="display">{t('pages.desk.display', 'Display')}</TabsTrigger>
                                <TabsTrigger value="audio">{t('pages.desk.audio', 'Audio')}</TabsTrigger>
                                <TabsTrigger value="advanced">{t('pages.desk.advanced', 'Advanced')}</TabsTrigger>
                            </TabsList>

                            {/* --- DISPLAY TAB --- */}
                            <TabsContent value="display" className="space-y-4 pt-4">
                                <FormField
                                    control={form.control}
                                    name="image_capture"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.screenCaptureMode', 'Screen Capture Mode')}</FormLabel>
                                            {/* Radix Select + RHF reset can miss value updates; re-mount to reflect initial data */}
                                            {(() => {
                                                const currentValue = field.value || ""
                                                return (
                                            <Select
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
                                                }}
                                                defaultValue={currentValue}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder={t('pages.desk.screenCaptureModePlaceholder', 'Select Capture Mode')} />
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
                                            {t("pages.desk.dxgiVideoBlackBarWarning.title", "DXGI capture limitation")}
                                        </AlertTitle>
                                        <AlertDescription>
                                            {t(
                                                "pages.desk.dxgiVideoBlackBarWarning.body",
                                                "DXGI captures DWM's framebuffer. Browser-decoded video uses a hardware overlay surface that DXGI cannot read, so brief black bars may flash while playing video. Switch to WGC for correct video capture."
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
                                                        "Previously selected display is unavailable",
                                                    )}
                                                </AlertTitle>
                                                <AlertDescription>
                                                    {t(
                                                        "pages.desk.displayDeviceStaleWarning.body",
                                                        "The display {{name}} you picked last time is no longer enumerated. We prefilled the primary monitor; pick a different one if needed.",
                                                        { name: staleSavedDeviceName },
                                                    )}
                                                </AlertDescription>
                                            </Alert>
                                        )}
                                        <FormField
                                            control={form.control}
                                            name="video_device_name"
                                            rules={{
                                                required: t(
                                                    "pages.desk.displayDeviceRequired",
                                                    "Please pick a display before connecting.",
                                                ),
                                                validate: (value: string | undefined) =>
                                                    (value !== undefined && value !== "") ||
                                                    t(
                                                        "pages.desk.displayDeviceRequired",
                                                        "Please pick a display before connecting.",
                                                    ),
                                            }}
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t('pages.desk.displayDevice', 'Display Device')}</FormLabel>
                                                    {(() => {
                                                        const currentValue = field.value ?? ""
                                                        return (
                                                    <Select
                                                        onValueChange={(value: string) => {
                                                            field.onChange(value)
                                                            setStaleSavedDeviceName(null)
                                                        }}
                                                        key={`video-device-${currentValue || "empty"}`}
                                                        defaultValue={currentValue}
                                                    >
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder={t('pages.desk.displayDevicePlaceholder', 'Select Device')} />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            {videoDeviceList.map((device) => (
                                                                <SelectItem
                                                                    key={device.device_name}
                                                                    value={device.device_name ?? ""}
                                                                >
                                                                    {formatDisplayLabel(device)}
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
                                                    onCheckedChange={field.onChange}
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel>
                                                    {t('pages.desk.showRemoteMouse', 'Show Remote Mouse')}
                                                </FormLabel>
                                            </div>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="adaptive_web_page_resolution"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-start space-x-3 space-y-0 p-2 rounded-md border">
                                            <FormControl>
                                                <Checkbox
                                                    checked={field.value}
                                                    onCheckedChange={field.onChange}
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel>
                                                    {t('pages.desk.adaptiveResolution', 'Adaptive Resolution')}
                                                </FormLabel>
                                            </div>
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="video_zoom_ratio"
                                    render={({ field }) => (
                                        <FormItem className="pt-2">
                                            <FormLabel className="flex justify-between">
                                                <span>{t('pages.desk.remoteResolutionScale', 'Resolution Scale')}</span>
                                                <span className="text-muted-foreground">{field.value}%</span>
                                            </FormLabel>
                                            <FormControl>
                                                <Slider
                                                    min={10}
                                                    max={100}
                                                    step={1}
                                                    value={[field.value || 100]}
                                                    onValueChange={(vals: number[]) => field.onChange(vals[0])}
                                                    className="py-2"
                                                />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
                            </TabsContent>

                            {/* --- AUDIO TAB --- */}
                            <TabsContent value="audio" className="space-y-4 pt-4">
                                <FormField
                                    control={form.control}
                                    name="enable_audio"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-start space-x-3 space-y-0 p-2 rounded-md border">
                                            <FormControl>
                                                <Checkbox
                                                    checked={!!field.value}
                                                    onCheckedChange={field.onChange}
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel>
                                                    {t('pages.desk.captureAudio', 'Capture Audio')}
                                                </FormLabel>
                                            </div>
                                        </FormItem>
                                    )}
                                />

                                {enableAudio && (
                                    <>
                                        <FormField
                                            control={form.control}
                                            name="audio_capture"
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t('pages.desk.audioCaptureMode', 'Audio Capture Mode')}</FormLabel>
                                                    {(() => {
                                                        const currentValue = field.value || ""
                                                        return (
                                                    <Select
                                                        onValueChange={field.onChange}
                                                        key={`audio-capture-${currentValue || "empty"}`}
                                                        defaultValue={currentValue}
                                                    >
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder={t('pages.desk.audioCaptureModePlaceholder', 'Select Audio Mode')} />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            {audioCaptureList.map((mode) => (
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

                                        {selectedAudioCapture && audioDeviceList && audioDeviceList.length > 0 && (
                                            <FormField
                                                control={form.control}
                                                name="audio_device"
                                                render={({ field }) => {
                                                    const stringValue = field.value ? JSON.stringify(field.value) : "";
                                                    return (
                                                        <FormItem>
                                                            <FormLabel>{t('pages.desk.audioDevice', 'Audio Device')}</FormLabel>
                                                            {(() => (
                                                            <Select
                                                                onValueChange={(val) => {
                                                                    try {
                                                                        field.onChange(JSON.parse(val));
                                                                    } catch (e) { /* ignore parse error */ }
                                                                }}
                                                                key={`audio-device-${stringValue || "empty"}`}
                                                                defaultValue={stringValue}
                                                            >
                                                                <FormControl>
                                                                    <SelectTrigger>
                                                                        <SelectValue placeholder={t('pages.desk.audioDevicePlaceholder', 'Select Audio Device')} />
                                                                    </SelectTrigger>
                                                                </FormControl>
                                                                <SelectContent>
                                                                    {Array.from(new Set(audioDeviceList.map(item => item.data_flow))).map(dataFlow => {
                                                                        const defaultDevice = { audio_data_flow: dataFlow, audio_device_id: null };
                                                                        const valStr = JSON.stringify(defaultDevice);
                                                                        return (
                                                                            <SelectItem key={`default-${dataFlow}`} value={valStr}>
                                                                                [{dataFlow}] Default Device
                                                                            </SelectItem>
                                                                        )
                                                                    })}

                                                                    {audioDeviceList.map((device) => {
                                                                        const optValue = { audio_data_flow: device.data_flow, audio_device_id: device.id };
                                                                        const valStr = JSON.stringify(optValue);
                                                                        const defaultLabel = device.default ? ' (Default)' : '';
                                                                        return (
                                                                            <SelectItem key={device.id || valStr} value={valStr}>
                                                                                [{device.data_flow}] {device.firendly_name} {defaultLabel}
                                                                            </SelectItem>
                                                                        )
                                                                    })}
                                                                </SelectContent>
                                                            </Select>
                                                            ))()}
                                                            <FormMessage />
                                                        </FormItem>
                                                    )
                                                }}
                                            />
                                        )}

                                        <FormField
                                            control={form.control}
                                            name="audio_encoder"
                                            render={({ field }) => (
                                                <FormItem>
                                                    <FormLabel>{t('pages.desk.audioEncoder', 'Audio Encoder')}</FormLabel>
                                                    {(() => {
                                                        const currentValue = field.value || "auto"
                                                        return (
                                                    <Select
                                                        onValueChange={(val) => field.onChange(val === "auto" ? null : val)}
                                                        key={`audio-encoder-${currentValue}`}
                                                        defaultValue={currentValue}
                                                    >
                                                        <FormControl>
                                                            <SelectTrigger>
                                                                <SelectValue placeholder={t('pages.desk.autoBackendControl', 'Auto')} />
                                                            </SelectTrigger>
                                                        </FormControl>
                                                        <SelectContent>
                                                            <SelectItem value="auto">{t('pages.desk.autoBackendControl', 'Auto')}</SelectItem>
                                                            {initData?.audio_encoder_list?.map((encoder) => (
                                                                <SelectItem key={encoder} value={encoder}>
                                                                    {encoder}
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
                                    </>
                                )}
                            </TabsContent>

                            {/* --- ADVANCED TAB --- */}
                            <TabsContent value="advanced" className="space-y-4 pt-4">
                                <FormField
                                    control={form.control}
                                    name="video_encoder"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.videoEncoder', 'Video Encoder')}</FormLabel>
                                            {(() => {
                                                const currentValue = field.value || "auto"
                                                return (
                                            <Select
                                                onValueChange={(val) => field.onChange(val === "auto" ? null : val)}
                                                key={`video-encoder-${currentValue}`}
                                                defaultValue={currentValue}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder={t('pages.desk.autoBackendControl', 'Auto')} />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="auto">{t('pages.desk.autoBackendControl', 'Auto')}</SelectItem>
                                                    {initData?.video_encoder_list?.map((encoder) => (
                                                        <SelectItem key={encoder} value={encoder}>
                                                            {encoder}
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

                                <FormField
                                    control={form.control}
                                    name="wayland_control_mode"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.waylandControlMode', 'Wayland Control Mode')}</FormLabel>
                                            {(() => {
                                                const currentValue = field.value || "auto"
                                                return (
                                            <Select
                                                onValueChange={(val) => field.onChange(val)}
                                                key={`wayland-control-${currentValue}`}
                                                defaultValue={currentValue}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder={t('pages.desk.autoBackendControl', 'Auto')} />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    <SelectItem value="auto">{t('pages.desk.autoBackendControl', 'Auto')}</SelectItem>
                                                    <SelectItem value="portal">portal</SelectItem>
                                                    <SelectItem value="uinput">uinput</SelectItem>
                                                    <SelectItem value="none">none</SelectItem>
                                                </SelectContent>
                                            </Select>
                                                )
                                            })()}
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="video_quality"
                                    render={({ field }) => (
                                        <FormItem className="pt-2">
                                            <FormLabel className="flex justify-between">
                                                <span>{t('pages.desk.videoQuality', 'Video Quality')} ({t('pages.desk.videoQualityDescription', '0-63, lower is better')})</span>
                                                <span className="text-muted-foreground">{field.value}</span>
                                            </FormLabel>
                                            <FormControl>
                                                <Slider
                                                    min={0}
                                                    max={63}
                                                    step={1}
                                                    value={[field.value ?? 22]}
                                                    onValueChange={(vals: number[]) => field.onChange(vals[0])}
                                                    className="py-2"
                                                />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                {/* Client-only adaptive video quality
                                    toggle. Lives outside the form
                                    because the value never crosses
                                    the signaling boundary — it only
                                    gates whether the browser-side
                                    stats observer issues
                                    UpdateDeskSettings(video_quality)
                                    based on packet loss / RTT. Default
                                    on. Plain div + Label here (NOT
                                    FormItem/FormControl/FormLabel),
                                    because those primitives call
                                    useFormField() and require an
                                    enclosing <FormField>. */}
                                <div className="flex flex-row items-start space-x-3 p-2 rounded-md border">
                                    <Checkbox
                                        id="adaptive-quality-toggle"
                                        checked={adaptiveQualityEnabled}
                                        onCheckedChange={(checked) =>
                                            onAdaptiveQualityChange(checked === true)
                                        }
                                    />
                                    <div className="space-y-1 leading-none">
                                        <Label htmlFor="adaptive-quality-toggle">
                                            {t('pages.desk.adaptiveQuality', 'Adaptive Video Quality')}
                                        </Label>
                                        <p className="text-xs text-muted-foreground">
                                            {t(
                                                'pages.desk.adaptiveQualityDescription',
                                                'Auto-tune video quality based on packet loss and RTT'
                                            )}
                                        </p>
                                    </div>
                                </div>

                                <FormField
                                    control={form.control}
                                    name="video_fps"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.maxFps', 'Max FPS')}</FormLabel>
                                            <FormControl>
                                                <Input
                                                    type="number"
                                                    min={1}
                                                    placeholder={t('pages.desk.autoBackendControl', 'Auto')}
                                                    {...field}
                                                    value={field.value ?? ""}
                                                    onChange={(e) => {
                                                        const val = e.target.value;
                                                        if (val === "") {
                                                            field.onChange(undefined);
                                                        } else {
                                                            const num = Number(val);
                                                            field.onChange(num <= 0 ? undefined : num);
                                                        }
                                                    }}
                                                />
                                            </FormControl>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />

                                <FormField
                                    control={form.control}
                                    name="enable_dirty_rect"
                                    render={({ field }) => (
                                        <FormItem className="flex flex-row items-start space-x-3 space-y-0 p-2 rounded-md border">
                                            <FormControl>
                                                <Checkbox
                                                    checked={field.value ?? true}
                                                    onCheckedChange={field.onChange}
                                                />
                                            </FormControl>
                                            <div className="space-y-1 leading-none">
                                                <FormLabel>
                                                    {t('pages.desk.enableDirtyRect', 'Enable Dirty Rect Optimisation')}
                                                </FormLabel>
                                                <p className="text-xs text-muted-foreground">
                                                    {t(
                                                        'pages.desk.enableDirtyRectDescription',
                                                        'Only re-encode changed regions of the screen. Turn off if you see transient black bars during animations.'
                                                    )}
                                                </p>
                                            </div>
                                        </FormItem>
                                    )}
                                />
                            </TabsContent>
                        </Tabs>

                        <DialogFooter className="pt-4">
                            <Button type="button" variant="outline" onClick={onCancel}>
                                {t('pages.desk.close', 'Cancel')}
                            </Button>
                            <Button type="submit">
                                {t('pages.desk.connect', 'Connect')}
                            </Button>
                        </DialogFooter>
                    </form>
                </Form>
            </DialogContent>
        </Dialog>
    )
}
