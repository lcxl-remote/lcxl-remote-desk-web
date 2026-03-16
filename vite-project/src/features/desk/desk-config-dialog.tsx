import { useEffect } from "react"
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
    Accordion,
    AccordionContent,
    AccordionItem,
    AccordionTrigger,
} from "@/components/ui/accordion"
import { Checkbox } from "@/components/ui/checkbox"
import { Slider } from "@/components/ui/slider"
import { Input } from "@/components/ui/input"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { AlertTriangle } from "lucide-react"
import type { InitSignalingData, DeskSettings } from "@/services/types"

interface DeskConfigDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
    initData: InitSignalingData | null
    onSubmit: (settings: DeskSettings) => void
    onCancel: () => void
}

export function DeskConfigDialog({
    open,
    onOpenChange,
    initData,
    onSubmit,
    onCancel
}: DeskConfigDialogProps) {
    const { t } = useTranslation()

    // Extend DeskSettings for form state
    type FormSettings = DeskSettings & { enable_audio: boolean }

    const form = useForm<FormSettings>({
        defaultValues: {
            image_capture: "",
            video_device_index: 0,
            show_mouse: true,
            adaptive_web_page_resolution: true,
            video_zoom_ratio: 100,
            video_quality: 22,
            enable_audio: false,
            video_encoder: null,
            audio_encoder: null,
            wayland_control_mode: "auto",
            video_fps: undefined,
        },
    })

    useEffect(() => {
        if (initData?.desk_settings) {
            form.reset({
                ...initData.desk_settings,
                show_mouse: initData.desk_settings.show_mouse ?? true,
                adaptive_web_page_resolution: initData.desk_settings.adaptive_web_page_resolution ?? true,
                video_zoom_ratio: initData.desk_settings.video_zoom_ratio ?? 100,
                video_quality: initData.desk_settings.video_quality ?? 22,
                wayland_control_mode: initData.desk_settings.wayland_control_mode ?? "auto",
            })
        }
    }, [initData, form])

    const imageCaptureList = initData && initData.video_device_list ? Object.keys(initData.video_device_list) : []
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
            video_device_index: Number(values.video_device_index),
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
            <DialogContent className="sm:max-w-[425px]" onInteractOutside={handleInteractOutside}>
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
                        <div className="space-y-4 py-4">
                            <FormField
                                control={form.control}
                                name="image_capture"
                                render={({ field }) => (
                                    <FormItem>
                                        <FormLabel>{t('pages.desk.screenCaptureMode', 'Screen Capture Mode')}</FormLabel>
                                        <Select
                                            onValueChange={(value: string) => {
                                                field.onChange(value)
                                                form.setValue("video_device_index", 0) // Reset device index when capture mode changes
                                            }}
                                            value={field.value || ""}
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
                                        <FormMessage />
                                    </FormItem>
                                )}
                            />

                            {selectedImageCapture && videoDeviceList && videoDeviceList.length > 0 && (
                                <FormField
                                    control={form.control}
                                    name="video_device_index"
                                    render={({ field }) => (
                                        <FormItem>
                                            <FormLabel>{t('pages.desk.displayDevice', 'Display Device')}</FormLabel>
                                            <Select
                                                onValueChange={field.onChange}
                                                value={field.value !== undefined ? String(field.value) : "0"}
                                            >
                                                <FormControl>
                                                    <SelectTrigger>
                                                        <SelectValue placeholder={t('pages.desk.displayDevicePlaceholder', 'Select Device')} />
                                                    </SelectTrigger>
                                                </FormControl>
                                                <SelectContent>
                                                    {videoDeviceList.map((device, index) => (
                                                        <SelectItem key={index} value={String(index)}>
                                                            {`${device.display_device_name} (${device.desktop_coordinates.right}x${device.desktop_coordinates.bottom})`}
                                                        </SelectItem>
                                                    ))}
                                                </SelectContent>
                                            </Select>
                                            <FormMessage />
                                        </FormItem>
                                    )}
                                />
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
                        </div>

                        {/* --- BASIC AUDIO CONFIGURATION SECTION --- */}
                        <div className="space-y-4 pt-4 border-t">
                            <h3 className="text-sm font-medium text-muted-foreground">{t('pages.desk.audioConfig', 'Audio Configuration')}</h3>

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
                                                <Select
                                                    onValueChange={field.onChange}
                                                    value={field.value || ""}
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
                                                <FormMessage />
                                            </FormItem>
                                        )}
                                    />

                                    {selectedAudioCapture && audioDeviceList && audioDeviceList.length > 0 && (
                                        <FormField
                                            control={form.control}
                                            name="audio_device"
                                            render={({ field }) => {
                                                // React Hook Form requires string values for selects, but the original structure used complex objects.
                                                // We stringify the entire value object to store it in the Select component
                                                const stringValue = field.value ? JSON.stringify(field.value) : "";
                                                return (
                                                    <FormItem>
                                                        <FormLabel>{t('pages.desk.audioDevice', 'Audio Device')}</FormLabel>
                                                        <Select
                                                            onValueChange={(val) => {
                                                                try {
                                                                    field.onChange(JSON.parse(val));
                                                                } catch (e) { /* ignore parse error */ }
                                                            }}
                                                            value={stringValue}
                                                        >
                                                            <FormControl>
                                                                <SelectTrigger>
                                                                    <SelectValue placeholder={t('pages.desk.audioDevicePlaceholder', 'Select Audio Device')} />
                                                                </SelectTrigger>
                                                            </FormControl>
                                                            <SelectContent>
                                                                {/* First list "Default Device" options per data flow */}
                                                                {Array.from(new Set(audioDeviceList.map(item => item.data_flow))).map(dataFlow => {
                                                                    const defaultDevice = { audio_data_flow: dataFlow, audio_device_id: null };
                                                                    const valStr = JSON.stringify(defaultDevice);
                                                                    return (
                                                                        <SelectItem key={`default-${dataFlow}`} value={valStr}>
                                                                            [{dataFlow}] Default Device
                                                                        </SelectItem>
                                                                    )
                                                                })}

                                                                {/* Then list all specific devices */}
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
                                                        <FormMessage />
                                                    </FormItem>
                                                )
                                            }}
                                        />
                                    )}
                                </>
                            )}
                        </div>

                        {/* --- ADVANCED OPTIONS --- */}
                        <Accordion type="single" collapsible className="w-full mt-4">
                            <AccordionItem value="advanced" className="border-t">
                                <AccordionTrigger className="text-sm font-medium text-muted-foreground hover:no-underline">
                                    {t('pages.desk.advanced', 'Advanced Options')}
                                </AccordionTrigger>
                                <AccordionContent className="space-y-4 pt-4">

                                    <FormField
                                        control={form.control}
                                        name="video_encoder"
                                        render={({ field }) => (
                                            <FormItem>
                                                <FormLabel>{t('pages.desk.videoEncoder', 'Video Encoder')}</FormLabel>
                                                <Select
                                                    onValueChange={(val) => field.onChange(val === "auto" ? null : val)}
                                                    value={field.value || "auto"}
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
                                                <Select
                                                    onValueChange={(val) => field.onChange(val)}
                                                    value={field.value || "auto"}
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

                                    {enableAudio && (
                                        <div className="space-y-4 pt-4 border-t">
                                            <h3 className="text-sm font-medium text-muted-foreground">{t('pages.desk.advancedAudio', 'Advanced Audio')}</h3>
                                            <FormField
                                                control={form.control}
                                                name="audio_encoder"
                                                render={({ field }) => (
                                                    <FormItem>
                                                        <FormLabel>{t('pages.desk.audioEncoder', 'Audio Encoder')}</FormLabel>
                                                        <Select
                                                            onValueChange={(val) => field.onChange(val === "auto" ? null : val)}
                                                            value={field.value || "auto"}
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
                                                        <FormMessage />
                                                    </FormItem>
                                                )}
                                            />
                                        </div>
                                    )}
                                </AccordionContent>
                            </AccordionItem>
                        </Accordion>
                        <DialogFooter>
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
