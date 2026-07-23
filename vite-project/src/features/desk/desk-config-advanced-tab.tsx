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
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select"
import { Slider } from "@/components/ui/slider"
import { TabsContent } from "@/components/ui/tabs"
import type { InitSignalingData } from "@/services/types"
import type { DeskConfigFormSettings } from "./desk-config-model"

type DeskConfigAdvancedTabProps = {
    adaptiveBitrateEnabled: boolean
    adaptiveQualityEnabled: boolean
    form: UseFormReturn<DeskConfigFormSettings>
    initData: InitSignalingData | null
    onAdaptiveBitrateChange: (enabled: boolean) => void
    onAdaptiveQualityChange: (enabled: boolean) => void
}

export function DeskConfigAdvancedTab({
    adaptiveBitrateEnabled,
    adaptiveQualityEnabled,
    form,
    initData,
    onAdaptiveBitrateChange,
    onAdaptiveQualityChange,
}: DeskConfigAdvancedTabProps) {
    const { t } = useTranslation()

    return (
        <TabsContent className="space-y-4 pt-4" value="advanced">
            <FormField
                control={form.control}
                name="video_encoder"
                render={({ field }) => {
                    const currentValue = field.value || "auto"
                    return (
                        <FormItem>
                            <FormLabel>{t("pages.desk.videoEncoder")}</FormLabel>
                            <Select
                                defaultValue={currentValue}
                                key={`video-encoder-${currentValue}`}
                                onValueChange={(value) => {
                                    field.onChange(value === "auto" ? null : value)
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
                                    {initData?.video_encoder_list?.map((encoder) => (
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

            <FormField
                control={form.control}
                name="wayland_control_mode"
                render={({ field }) => {
                    const currentValue = field.value || "auto"
                    return (
                        <FormItem>
                            <FormLabel>{t("pages.desk.waylandControlMode")}</FormLabel>
                            <Select
                                defaultValue={currentValue}
                                key={`wayland-control-${currentValue}`}
                                onValueChange={field.onChange}
                            >
                                <FormControl>
                                    <SelectTrigger>
                                        <SelectValue placeholder={t("pages.desk.autoBackendControl")} />
                                    </SelectTrigger>
                                </FormControl>
                                <SelectContent>
                                    <SelectItem value="auto">{t("pages.desk.autoBackendControl")}</SelectItem>
                                    <SelectItem value="portal">portal</SelectItem>
                                    <SelectItem value="uinput">uinput</SelectItem>
                                    <SelectItem value="none">none</SelectItem>
                                </SelectContent>
                            </Select>
                            <FormMessage />
                        </FormItem>
                    )
                }}
            />

            <FormField
                control={form.control}
                name="video_quality"
                render={({ field }) => (
                    <FormItem className="pt-2">
                        <FormLabel className="flex justify-between">
                            <span>
                                {t("pages.desk.videoQuality")} ({t("pages.desk.videoQualityDescription")})
                            </span>
                            <span className="text-muted-foreground">{field.value}</span>
                        </FormLabel>
                        <FormControl>
                            <Slider
                                className="py-2"
                                max={63}
                                min={0}
                                onValueChange={(values) => field.onChange(values[0])}
                                step={1}
                                value={[field.value ?? 22]}
                            />
                        </FormControl>
                        <FormMessage />
                    </FormItem>
                )}
            />

            <div className="flex flex-row items-start space-x-3 rounded-md border p-2">
                <Checkbox
                    checked={adaptiveQualityEnabled}
                    id="adaptive-quality-toggle"
                    onCheckedChange={(checked) => {
                        onAdaptiveQualityChange(checked === true)
                    }}
                />
                <div className="space-y-1 leading-none">
                    <Label htmlFor="adaptive-quality-toggle">
                        {t("pages.desk.adaptiveQuality")}
                    </Label>
                    <p className="text-xs text-muted-foreground">
                        {t("pages.desk.adaptiveQualityDescription")}
                    </p>
                </div>
            </div>

            <div className="flex flex-row items-start space-x-3 rounded-md border p-2">
                <Checkbox
                    checked={adaptiveBitrateEnabled}
                    id="adaptive-bitrate-toggle"
                    onCheckedChange={(checked) => {
                        onAdaptiveBitrateChange(checked === true)
                    }}
                />
                <div className="space-y-1 leading-none">
                    <Label htmlFor="adaptive-bitrate-toggle">
                        {t("pages.desk.adaptiveBitrateCap")}
                    </Label>
                    <p className="text-xs text-muted-foreground">
                        {t("pages.desk.adaptiveBitrateCapDescription")}
                    </p>
                </div>
            </div>

            <FormField
                control={form.control}
                name="video_fps"
                render={({ field }) => (
                    <FormItem>
                        <FormLabel>{t("pages.desk.maxFps")}</FormLabel>
                        <FormControl>
                            <Input
                                {...field}
                                min={1}
                                onChange={(event) => {
                                    const value = event.target.value
                                    if (value === "") {
                                        field.onChange(undefined)
                                        return
                                    }
                                    const number = Number(value)
                                    field.onChange(number <= 0 ? undefined : number)
                                }}
                                placeholder={t("pages.desk.autoBackendControl")}
                                type="number"
                                value={field.value ?? ""}
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
                    <FormItem className="flex flex-row items-start space-y-0 space-x-3 rounded-md border p-2">
                        <FormControl>
                            <Checkbox
                                checked={field.value ?? true}
                                onCheckedChange={field.onChange}
                            />
                        </FormControl>
                        <div className="space-y-1 leading-none">
                            <FormLabel>{t("pages.desk.enableDirtyRect")}</FormLabel>
                            <p className="text-xs text-muted-foreground">
                                {t("pages.desk.enableDirtyRectDescription")}
                            </p>
                        </div>
                    </FormItem>
                )}
            />
        </TabsContent>
    )
}
