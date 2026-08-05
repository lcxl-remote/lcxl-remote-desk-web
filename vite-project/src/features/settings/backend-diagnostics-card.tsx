import { useTranslation } from "react-i18next"
import { Alert, AlertDescription } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import type { BackendInfo } from "@/services/types"

type DiagnosticSection = BackendInfo["platform_diagnostics"][number]
type DiagnosticItem = DiagnosticSection["items"][number]

function readableKey(key: string): string {
    return key
        .split("_")
        .filter(Boolean)
        .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
        .join(" ")
}

function itemLabel(key: string, t: ReturnType<typeof useTranslation>["t"]): string {
    const known: Record<string, string> = {
        wayland_display: "WAYLAND_DISPLAY",
        x11_display: "DISPLAY",
        remote_desktop_input: t("pages.system.settings.backendDiagnostics.remoteDesktopInput"),
        screencast_portal: t("pages.system.settings.backendDiagnostics.screenCastPortal"),
    }
    return known[key] ?? readableKey(key)
}

function statusVariant(status: DiagnosticItem["status"]): "default" | "secondary" | "destructive" | "outline" {
    switch (status) {
        case "ready": return "default"
        case "warning": return "outline"
        case "error": return "destructive"
        default: return "secondary"
    }
}

export function BackendDiagnosticsCard({
    sections,
}: {
    sections: BackendInfo["platform_diagnostics"] | undefined
}) {
    const { t } = useTranslation()
    if (!sections?.length) return null

    return (
        <div className="space-y-4">
            {sections.map((section) => (
                <section className="space-y-2" key={`${section.platform}:${section.key}`}>
                    <h4 className="font-medium">
                        {section.key === "linux_display"
                            ? t("pages.system.settings.backendDiagnostics.linuxDisplay")
                            : readableKey(section.key)}
                    </h4>
                    {section.items.map((item) => (
                        <div className="space-y-1" key={item.key}>
                            <div className="flex items-center justify-between gap-3">
                                <span>{itemLabel(item.key, t)}</span>
                                <Badge variant={statusVariant(item.status)}>{item.value}</Badge>
                            </div>
                            {item.detail && (
                                <Alert variant={item.status === "error" ? "destructive" : "default"}>
                                    <AlertDescription>{item.detail}</AlertDescription>
                                </Alert>
                            )}
                        </div>
                    ))}
                </section>
            ))}
        </div>
    )
}
