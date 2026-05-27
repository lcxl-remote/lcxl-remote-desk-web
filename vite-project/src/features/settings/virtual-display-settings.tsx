import * as React from "react"
import type { TFunction } from "i18next"
import { useTranslation } from "react-i18next"
import { Loader2, RefreshCw } from "lucide-react"

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Switch } from "@/components/ui/switch"
import { useToast } from "@/hooks/use-toast"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { ServiceUninstallDialog } from "@/features/layout/service-uninstall-dialog"

interface DriverStatus {
    files_available: boolean
    files_dir: string | null
    installed: boolean | null
    installed_oem_infs: string[] | null
    can_modify: boolean
}

/**
 * UI wire type mirroring server's `VirtualDisplaySettings`. Fields
 * match the generated `@/services/types` shape, but `u64` numeric
 * fields are `number` here instead of `bigint`: the wire format is
 * JSON (Number.MAX_SAFE_INTEGER easily accommodates the ms ranges),
 * and `JSON.stringify(5000n)` throws TypeError. See generated
 * `services/types.ts` + server `model/settings/virtual_display.rs::Default`
 * for the authoritative definitions.
 */
interface VirtualDisplaySettings {
    enabled: boolean
    exclusive: boolean
    prompt_ms: number
    adaptive_debounce_ms: number
    adaptive_throttle_ms: number
    adaptive_min_delta_px: number
}

/**
 * Mirrors server's `VirtualDisplaySettings::default()`. Used as the
 * fixture in unit tests and as the defensive base inside
 * `saveSettings` when local `settings` is still `null` (initial load
 * race). NOT used as a GET-failure fallback — see `settingsLoadFailed`.
 */
const DEFAULT_VIRTUAL_DISPLAY_SETTINGS: VirtualDisplaySettings = {
    enabled: false,
    exclusive: false,
    prompt_ms: 5000,
    adaptive_debounce_ms: 5000,
    adaptive_throttle_ms: 1000,
    adaptive_min_delta_px: 16,
}

/**
 * Driver status badge — covers four states. The order matters: an
 * "unknown" / "not installed" state can coexist with `files_available`,
 * but the lack of staged driver files is the most actionable signal
 * the user can fix, so it wins the badge.
 */
function DriverBadge({ status }: { status: DriverStatus | null }) {
    const { t } = useTranslation()
    if (!status) {
        return (
            <Badge variant="outline">
                {t("pages.virtualDisplay.driver.badge.loading", "Loading…")}
            </Badge>
        )
    }
    if (!status.files_available) {
        return (
            <Badge variant="destructive">
                {t("pages.virtualDisplay.driver.badge.filesMissing", "Driver files missing")}
            </Badge>
        )
    }
    if (status.installed === null) {
        return (
            <Badge variant="outline">
                {t("pages.virtualDisplay.driver.badge.unknown", "Status unknown")}
            </Badge>
        )
    }
    if (status.installed === false) {
        return (
            <Badge variant="secondary">
                {t("pages.virtualDisplay.driver.badge.notInstalled", "Not installed")}
            </Badge>
        )
    }
    const oems = status.installed_oem_infs ?? []
    return (
        <Badge variant="default">
            {t("pages.virtualDisplay.driver.badge.installed", "Installed")}
            {oems.length > 0 ? ` (${oems.join(", ")})` : ""}
        </Badge>
    )
}

export function VirtualDisplaySettings() {
    const { t } = useTranslation()
    const { toast } = useToast()
    const { data: serverInfoResp } = useQueryServerInfo()
    const serverInfo = serverInfoResp?.data

    const [status, setStatus] = React.useState<DriverStatus | null>(null)
    const [statusLoading, setStatusLoading] = React.useState(false)
    const [busy, setBusy] = React.useState(false)
    const [settings, setSettings] = React.useState<VirtualDisplaySettings | null>(null)
    const [settingsLoading, setSettingsLoading] = React.useState(true)
    // Failure flag for the settings GET. When `true` we keep
    // `settings === null` and disable every save control to avoid
    // overwriting the unknown server-side config with defaults
    // (which would happen if we naively fell back to
    // `DEFAULT_VIRTUAL_DISPLAY_SETTINGS` on transient errors).
    const [settingsLoadFailed, setSettingsLoadFailed] = React.useState(false)
    const [uninstallDialogOpen, setUninstallDialogOpen] = React.useState(false)
    // Local buffer for the prompt_ms input — driving it directly off
    // `settings.prompt_ms` would force every keystroke through a save
    // cycle. A useEffect below syncs it whenever the canonical
    // settings change (first GET / save round-trip / uninstall).
    const [promptMsInput, setPromptMsInput] = React.useState("5000")

    const refreshStatus = React.useCallback(async () => {
        setStatusLoading(true)
        try {
            const resp = await fetch("/api/virtual-display/driver/status")
            const body = await resp.json()
            if (body?.code === 0 && body.data) {
                setStatus(body.data as DriverStatus)
            } else {
                setStatus(null)
            }
        } catch {
            setStatus(null)
        } finally {
            setStatusLoading(false)
        }
    }, [])

    const loadSettings = React.useCallback(async () => {
        setSettingsLoading(true)
        try {
            const resp = await fetch("/api/desk/settings/virtual-display")
            const body = await resp.json()
            if (body?.code === 0 && body.data) {
                setSettings(body.data as VirtualDisplaySettings)
                setSettingsLoadFailed(false)
            } else {
                // Backend returned a non-zero code or empty body. We do
                // NOT know what's actually persisted, so refuse to
                // pretend it's defaults — flip the failure flag and
                // surface a retry path in the UI.
                setSettings(null)
                setSettingsLoadFailed(true)
            }
        } catch {
            // Network / parse failure: same treatment as above.
            setSettings(null)
            setSettingsLoadFailed(true)
        } finally {
            setSettingsLoading(false)
        }
    }, [])

    React.useEffect(() => {
        refreshStatus()
        loadSettings()
    }, [refreshStatus, loadSettings])

    // Keep the local prompt_ms input text in sync with the canonical
    // settings whenever it changes (first GET / save round-trip /
    // uninstall reload). Without this, after the server clamps a
    // submitted value the input would keep displaying the stale
    // pre-clamp text.
    React.useEffect(() => {
        if (settings) {
            setPromptMsInput(String(settings.prompt_ms))
        }
    }, [settings?.prompt_ms])

    const isServiceDaemon = serverInfo?.startup_mode === "service-daemon"
    const canModify = status?.can_modify === true

    const handleInstall = async () => {
        setBusy(true)
        try {
            const resp = await fetch("/api/virtual-display/driver/install", {
                method: "POST",
            })
            const body = await resp.json()
            if (body?.code === 0) {
                toast({
                    title: t("pages.system.settings.success", "Success"),
                    description: t(
                        "pages.virtualDisplay.driver.installSuccess",
                        "Driver installed.",
                    ),
                })
                setStatus(body.data as DriverStatus)
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error", "Error"),
                    description: errorMessage(body?.code, body?.message, t),
                })
            }
        } finally {
            setBusy(false)
        }
    }

    const handleUninstall = async () => {
        setBusy(true)
        try {
            const resp = await fetch("/api/virtual-display/driver/uninstall", {
                method: "POST",
            })
            const body = await resp.json()
            if (body?.code === 0) {
                toast({
                    title: t("pages.system.settings.success", "Success"),
                    description: t(
                        "pages.virtualDisplay.driver.uninstallSuccess",
                        "Driver uninstalled.",
                    ),
                })
                setStatus(body.data as DriverStatus)
                // The backend uninstall path only flips `enabled=false`;
                // exclusive / prompt_ms / adaptive_* are preserved. Pull
                // a fresh GET so the local state reflects that — a local
                // `{ enabled: false }` reset would briefly show wrong
                // values for everything else until the next page load.
                await loadSettings()
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error", "Error"),
                    description: errorMessage(body?.code, body?.message, t),
                })
            }
        } finally {
            setBusy(false)
        }
    }

    /**
     * Save a partial settings patch by merging with the current
     * `settings` and POSTing the full struct. Required because the
     * server deserialises the body as `VirtualDisplaySettings` — any
     * field absent from the payload reverts to `Default`, which
     * would silently reset adaptive_* / exclusive / prompt_ms every
     * time the user toggled enabled.
     */
    const saveSettings = async (patch: Partial<VirtualDisplaySettings>) => {
        const base = settings ?? DEFAULT_VIRTUAL_DISPLAY_SETTINGS
        const payload: VirtualDisplaySettings = { ...base, ...patch }
        setBusy(true)
        try {
            const resp = await fetch("/api/desk/settings/virtual-display", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(payload),
            })
            const body = await resp.json()
            if (body?.code === 0) {
                setSettings(body.data as VirtualDisplaySettings)
                setSettingsLoadFailed(false)
                toast({
                    title: t("pages.system.settings.success", "Success"),
                    description: t(
                        "pages.virtualDisplay.enabled.saved",
                        "Virtual display preference saved.",
                    ),
                })
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error", "Error"),
                    description: errorMessage(body?.code, body?.message, t),
                })
            }
        } finally {
            setBusy(false)
        }
    }

    const enabledSwitchDisabled =
        busy || settingsLoading || settingsLoadFailed || status?.installed !== true

    /** Exclusive mode requires the virtual display to be enabled
     *  first — there is nothing to flip displays to otherwise. */
    const exclusiveControlsDisabled =
        enabledSwitchDisabled || settings?.enabled !== true

    /** Parse + clamp + persist the prompt_ms input. Empty string or
     *  non-numeric inputs revert the local buffer to the canonical
     *  value WITHOUT firing a POST — that lets the user backspace the
     *  field mid-edit without surprises. */
    const commitPromptMs = () => {
        if (!settings) return
        const raw = promptMsInput.trim()
        const parsed = Number(raw)
        if (raw === "" || !Number.isFinite(parsed)) {
            setPromptMsInput(String(settings.prompt_ms))
            return
        }
        const clamped = Math.min(60000, Math.max(0, Math.floor(parsed)))
        setPromptMsInput(String(clamped))
        if (clamped !== settings.prompt_ms) {
            void saveSettings({ prompt_ms: clamped })
        }
    }

    return (
        <div className="container mx-auto max-w-4xl py-8 space-y-6">
            <div>
                <h1 className="text-3xl font-bold tracking-tight">
                    {t("pages.virtualDisplay.title", "Virtual Display")}
                </h1>
                <p className="text-muted-foreground">
                    {t(
                        "pages.virtualDisplay.description",
                        "Manage the Windows IDD virtual display driver and toggle whether the daemon creates a virtual monitor at startup.",
                    )}
                </p>
            </div>

            <Card>
                <CardHeader className="flex flex-row items-start justify-between gap-2">
                    <div>
                        <CardTitle>
                            {t("pages.virtualDisplay.driver.title", "Driver status")}
                        </CardTitle>
                        <CardDescription>
                            {t(
                                "pages.virtualDisplay.driver.description",
                                "The LcxlVirtualDisplay IDD driver must be staged before the virtual display can be enabled.",
                            )}
                        </CardDescription>
                    </div>
                    <Button
                        variant="ghost"
                        size="sm"
                        onClick={refreshStatus}
                        disabled={statusLoading}
                    >
                        {statusLoading ? (
                            <Loader2 className="h-4 w-4 animate-spin" />
                        ) : (
                            <RefreshCw className="h-4 w-4" />
                        )}
                    </Button>
                </CardHeader>
                <CardContent className="space-y-4">
                    <div className="flex items-center justify-between">
                        <span className="text-sm font-medium">
                            {t("pages.virtualDisplay.driver.statusLabel", "Status")}
                        </span>
                        <DriverBadge status={status} />
                    </div>

                    <div className="space-y-2">
                        <Label htmlFor="vdd-files-dir">
                            {t(
                                "pages.virtualDisplay.driver.filesDirLabel",
                                "Driver files directory",
                            )}
                        </Label>
                        <Input
                            id="vdd-files-dir"
                            readOnly
                            value={status?.files_dir ?? ""}
                            placeholder={t(
                                "pages.virtualDisplay.driver.filesDirUnknown",
                                "Unknown",
                            )}
                        />
                    </div>

                    {!canModify && (
                        <Alert>
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.driver.cannotModifyTitle",
                                    "Driver changes not permitted",
                                )}
                            </AlertTitle>
                            <AlertDescription>
                                {isServiceDaemon
                                    ? t(
                                          "pages.virtualDisplay.driver.cannotModifyDaemon",
                                          "Daemon refused the request — check the daemon log.",
                                      )
                                    : t(
                                          "pages.virtualDisplay.driver.cannotModifyDefault",
                                          "In portable mode, install/uninstall the IDD driver via the \"Install Service\" flow; uninstalling the service force-removes the driver.",
                                      )}
                            </AlertDescription>
                        </Alert>
                    )}

                    {canModify && !status?.files_available && (
                        <Alert variant="destructive">
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.driver.filesMissingTitle",
                                    "Driver files missing",
                                )}
                            </AlertTitle>
                            <AlertDescription>
                                {t(
                                    "pages.virtualDisplay.driver.filesMissingDescription",
                                    "Place the LcxlVirtualDisplay driver files under <exe_dir>/drivers/LcxlVirtualDisplay/ and refresh.",
                                )}
                            </AlertDescription>
                        </Alert>
                    )}

                    {canModify && status?.installed === null && (
                        <Alert>
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.driver.unknownTitle",
                                    "Driver status unknown",
                                )}
                            </AlertTitle>
                            <AlertDescription>
                                {t(
                                    "pages.virtualDisplay.driver.unknownDescription",
                                    "Could not query Get-WindowsDriver or pnputil. Retry the status check, ideally as an administrator.",
                                )}
                            </AlertDescription>
                        </Alert>
                    )}

                    <div className="flex gap-2">
                        <Button
                            onClick={handleInstall}
                            disabled={
                                busy ||
                                !canModify ||
                                !status?.files_available ||
                                status?.installed === true ||
                                status?.installed === null
                            }
                        >
                            {t("pages.virtualDisplay.driver.installButton", "Install driver")}
                        </Button>
                        <Button
                            variant="destructive"
                            onClick={handleUninstall}
                            disabled={
                                busy ||
                                !canModify ||
                                status?.installed !== true
                            }
                        >
                            {t(
                                "pages.virtualDisplay.driver.uninstallButton",
                                "Uninstall driver",
                            )}
                        </Button>
                    </div>
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>
                        {t("pages.virtualDisplay.enabled.title", "Enable virtual display")}
                    </CardTitle>
                    <CardDescription>
                        {t(
                            "pages.virtualDisplay.enabled.description",
                            "When enabled, the daemon will create the IDD virtual monitor on startup.",
                        )}
                    </CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    {!isServiceDaemon && (
                        <Alert>
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.enabled.notDaemonTitle",
                                    "Only effective in service-daemon mode",
                                )}
                            </AlertTitle>
                            <AlertDescription>
                                {t(
                                    "pages.virtualDisplay.enabled.notDaemonDescription",
                                    "The flag is saved but only acted on when running as the Windows service.",
                                )}
                            </AlertDescription>
                        </Alert>
                    )}
                    {settingsLoadFailed && (
                        <Alert variant="destructive">
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.loadFailedTitle",
                                    "Failed to load settings",
                                )}
                            </AlertTitle>
                            <AlertDescription className="flex items-center justify-between gap-2">
                                <span>
                                    {t(
                                        "pages.virtualDisplay.loadFailedDescription",
                                        "Could not retrieve the current configuration — saving is disabled to avoid overwriting unknown values.",
                                    )}
                                </span>
                                <Button
                                    size="sm"
                                    variant="outline"
                                    onClick={loadSettings}
                                    disabled={settingsLoading}
                                >
                                    {t("pages.virtualDisplay.loadFailedRetry", "Retry")}
                                </Button>
                            </AlertDescription>
                        </Alert>
                    )}
                    <div className="flex items-center justify-between rounded-lg border p-3">
                        <div className="space-y-0.5">
                            <Label htmlFor="vdd-enabled" className="font-medium">
                                {t(
                                    "pages.virtualDisplay.enabled.switchLabel",
                                    "Create virtual monitor on startup",
                                )}
                            </Label>
                            <p className="text-xs text-muted-foreground">
                                {status?.installed === null
                                    ? t(
                                          "pages.virtualDisplay.enabled.helperUnknown",
                                          "Driver status unknown — refresh and retry.",
                                      )
                                    : status?.installed === false
                                      ? t(
                                            "pages.virtualDisplay.enabled.helperNotInstalled",
                                            "Driver is not installed; install it first.",
                                        )
                                      : t(
                                            "pages.virtualDisplay.enabled.helperReady",
                                            "Driver is installed; this flag controls whether the daemon brings the monitor up.",
                                        )}
                            </p>
                        </div>
                        <Switch
                            id="vdd-enabled"
                            checked={settings?.enabled === true}
                            onCheckedChange={(enabled) => saveSettings({ enabled })}
                            disabled={enabledSwitchDisabled}
                        />
                    </div>

                    {!isServiceDaemon && (
                        <Button
                            variant="outline"
                            onClick={() => setUninstallDialogOpen(true)}
                            disabled={
                                busy ||
                                serverInfo?.service_installed !== true ||
                                !serverInfo?.is_admin
                            }
                        >
                            {t(
                                "pages.system.settings.serviceManagement.uninstall",
                                "Uninstall service",
                            )}
                        </Button>
                    )}
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>
                        {t("pages.virtualDisplay.exclusive.title", "Exclusive mode")}
                    </CardTitle>
                    <CardDescription>
                        {t(
                            "pages.virtualDisplay.exclusive.description",
                            "When the remote peer acquires control, detach all physical displays so Windows migrates windows onto the virtual one. Restores automatically on release / disconnect.",
                        )}
                    </CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    {!isServiceDaemon && (
                        <Alert>
                            <AlertTitle>
                                {t(
                                    "pages.virtualDisplay.exclusive.notDaemonTitle",
                                    "Only effective in service-daemon mode",
                                )}
                            </AlertTitle>
                            <AlertDescription>
                                {t(
                                    "pages.virtualDisplay.exclusive.notDaemonDescription",
                                    "The setting is saved but only acted on when running as the Windows service.",
                                )}
                            </AlertDescription>
                        </Alert>
                    )}
                    <div className="flex items-center justify-between rounded-lg border p-3">
                        <div className="space-y-0.5">
                            <Label htmlFor="vdd-exclusive" className="font-medium">
                                {t(
                                    "pages.virtualDisplay.exclusive.toggleLabel",
                                    "Enable exclusive mode",
                                )}
                            </Label>
                            <p className="text-xs text-muted-foreground">
                                {t(
                                    "pages.virtualDisplay.exclusive.toggleHelper",
                                    "Requires virtual display to be enabled first",
                                )}
                            </p>
                        </div>
                        <Switch
                            id="vdd-exclusive"
                            checked={settings?.exclusive === true}
                            onCheckedChange={(exclusive) => saveSettings({ exclusive })}
                            disabled={exclusiveControlsDisabled}
                        />
                    </div>
                    <div className="space-y-2 rounded-lg border p-3">
                        <Label htmlFor="vdd-prompt-ms">
                            {t(
                                "pages.virtualDisplay.exclusive.promptMsLabel",
                                "Pre-switch prompt duration (ms)",
                            )}
                        </Label>
                        <Input
                            id="vdd-prompt-ms"
                            type="number"
                            min={0}
                            max={60000}
                            step={500}
                            value={promptMsInput}
                            onChange={(e) => setPromptMsInput(e.target.value)}
                            onBlur={commitPromptMs}
                            disabled={exclusiveControlsDisabled}
                        />
                        <p className="text-xs text-muted-foreground">
                            {t(
                                "pages.virtualDisplay.exclusive.promptMsHelper",
                                "0 – 60000 ms (0 = skip prompt)",
                            )}
                        </p>
                    </div>
                </CardContent>
            </Card>

            <ServiceUninstallDialog
                open={uninstallDialogOpen}
                onOpenChange={setUninstallDialogOpen}
            />
        </div>
    )
}

function errorMessage(
    code: number | undefined,
    fallback: string | undefined,
    t: TFunction,
): string {
    switch (code) {
        case 4:
            return t(
                "pages.virtualDisplay.error.permissionError",
                "Administrator permission required.",
            )
        case 8:
            return t(
                "pages.virtualDisplay.error.preconditionFailed",
                "Driver is not staged; install it first.",
            )
        case 11:
            return t(
                "pages.virtualDisplay.error.fileNotFound",
                "Driver files not found next to the server binary.",
            )
        default:
            return (
                fallback ??
                t("pages.virtualDisplay.error.generic", "Operation failed.")
            )
    }
}
