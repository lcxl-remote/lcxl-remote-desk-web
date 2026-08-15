import type { UseFormReturn } from "react-hook-form"
import { useTranslation } from "react-i18next"

import { Checkbox } from "@/components/ui/checkbox"
import {
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
import { TabsContent } from "@/components/ui/tabs"
import type { RemoteAccessInitializedData } from "@/services/types"
import type { DeskConfigFormSettings } from "./desk-config-model"

type DeskConfigAudioTabProps = {
    form: UseFormReturn<DeskConfigFormSettings>
    initData: RemoteAccessInitializedData | null
    systemAudioAllowed: boolean
}

export function DeskConfigAudioTab({
    form,
    initData,
    systemAudioAllowed,
}: DeskConfigAudioTabProps) {
    const { t } = useTranslation()
    const enableAudio = form.watch("enable_audio")
    const audioCaptureList = Object.keys(initData?.audio_device_list ?? {})
    const audioCapabilityAvailable = systemAudioAllowed && audioCaptureList.length > 0
        && (initData?.audio_encoder_list?.length ?? 0) > 0
    const selectedAudioCapture = form.watch("audio_capture")
    const audioDeviceList = selectedAudioCapture
        ? initData?.audio_device_list?.[selectedAudioCapture] ?? []
        : []

    return (
        <TabsContent className="space-y-4 pt-4" value="audio">
            <FormField
                control={form.control}
                name="enable_audio"
                render={({ field }) => (
                    <FormItem className="flex flex-row items-start space-y-0 space-x-3 rounded-md border p-2">
                        <FormControl>
                            <Checkbox
                                checked={!!field.value}
                                disabled={!audioCapabilityAvailable}
                                onCheckedChange={field.onChange}
                            />
                        </FormControl>
                        <div className="space-y-1 leading-none">
                            <FormLabel>{t("pages.desk.captureAudio")}</FormLabel>
                        </div>
                    </FormItem>
                )}
            />

            {enableAudio && (
                <>
                    <FormField
                        control={form.control}
                        name="audio_capture"
                        render={({ field }) => {
                            const currentValue = field.value || ""
                            return (
                                <FormItem>
                                    <FormLabel>{t("pages.desk.audioCaptureMode")}</FormLabel>
                                    <Select
                                        defaultValue={currentValue}
                                        key={`audio-capture-${currentValue || "empty"}`}
                                        onValueChange={field.onChange}
                                    >
                                        <FormControl>
                                            <SelectTrigger>
                                                <SelectValue placeholder={t("pages.desk.audioCaptureModePlaceholder")} />
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
                            )
                        }}
                    />

                    {selectedAudioCapture && audioDeviceList.length > 0 && (
                        <FormField
                            control={form.control}
                            name="audio_device"
                            render={({ field }) => {
                                const stringValue = field.value
                                    ? JSON.stringify(field.value)
                                    : ""
                                return (
                                    <FormItem>
                                        <FormLabel>{t("pages.desk.audioDevice")}</FormLabel>
                                        <Select
                                            defaultValue={stringValue}
                                            key={`audio-device-${stringValue || "empty"}`}
                                            onValueChange={(value) => {
                                                try {
                                                    field.onChange(JSON.parse(value))
                                                } catch {
                                                    // Ignore malformed values from the UI boundary.
                                                }
                                            }}
                                        >
                                            <FormControl>
                                                <SelectTrigger>
                                                    <SelectValue placeholder={t("pages.desk.audioDevicePlaceholder")} />
                                                </SelectTrigger>
                                            </FormControl>
                                            <SelectContent>
                                                {Array.from(new Set(
                                                    audioDeviceList.map((item) => item.data_flow),
                                                )).map((dataFlow) => {
                                                    const value = JSON.stringify({
                                                        audio_data_flow: dataFlow,
                                                        audio_device_id: null,
                                                    })
                                                    return (
                                                        <SelectItem key={`default-${dataFlow}`} value={value}>
                                                            [{dataFlow}] Default Device
                                                        </SelectItem>
                                                    )
                                                })}
                                                {audioDeviceList.map((device) => {
                                                    const value = JSON.stringify({
                                                        audio_data_flow: device.data_flow,
                                                        audio_device_id: device.id,
                                                    })
                                                    return (
                                                        <SelectItem key={device.id || value} value={value}>
                                                            [{device.data_flow}] {device.firendly_name}
                                                            {device.default ? " (Default)" : ""}
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

                    <FormField
                        control={form.control}
                        name="audio_encoder"
                        render={({ field }) => {
                            const currentValue = field.value || "auto"
                            return (
                                <FormItem>
                                    <FormLabel>{t("pages.desk.audioEncoder")}</FormLabel>
                                    <Select
                                        defaultValue={currentValue}
                                        key={`audio-encoder-${currentValue}`}
                                        onValueChange={(value) => {
                                            field.onChange(value === "auto" ? null : "Opus")
                                        }}
                                    >
                                        <FormControl>
                                            <SelectTrigger>
                                                <SelectValue placeholder={t("pages.desk.autoBackendControl")} />
                                            </SelectTrigger>
                                        </FormControl>
                                        <SelectContent>
                                            <SelectItem value="auto">
                                                {t("pages.desk.autoBackendControl")}
                                            </SelectItem>
                                            {initData?.audio_encoder_list?.map((encoder) => (
                                                <SelectItem key={encoder} value={encoder}>
                                                    {encoder}
                                                </SelectItem>
                                            ))}
                                        </SelectContent>
                                    </Select>
                                    <FormMessage />
                                </FormItem>
                            )
                        }}
                    />
                </>
            )}
        </TabsContent>
    )
}
