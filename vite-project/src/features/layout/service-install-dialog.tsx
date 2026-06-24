import * as React from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useToast } from "@/hooks/use-toast"

/**
 * Shared "install service" confirmation dialog. Used by both the
 * layout banner and the system-settings page so the two entry points
 * present an identical UX (the system-settings page previously skipped
 * the confirmation step entirely).
 *
 * The dialog fetches `/api/virtual-display/driver/status` on open so
 * the "also install IDD virtual display driver" checkbox can be
 * disabled (with an inline hint) when the driver files are not
 * present next to the server binary.
 */
export interface ServiceInstallDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
    defaultInstallPath: string
}

interface DriverStatus {
    files_available: boolean
    files_dir: string | null
    installed: boolean | null
    installed_oem_infs: string[] | null
    can_modify: boolean
}

export function ServiceInstallDialog(props: ServiceInstallDialogProps) {
    const { open, onOpenChange, defaultInstallPath } = props
    const { t } = useTranslation()
    const { toast } = useToast()

    const [installPath, setInstallPath] = React.useState(defaultInstallPath)
    const [installIdd, setInstallIdd] = React.useState(false)
    const [submitting, setSubmitting] = React.useState(false)
    const [driverStatus, setDriverStatus] = React.useState<DriverStatus | null>(null)
    const [statusLoading, setStatusLoading] = React.useState(false)

    React.useEffect(() => {
        if (open) {
            setInstallPath(defaultInstallPath)
            setInstallIdd(false)
            setStatusLoading(true)
            fetch("/api/virtual-display/driver/status")
                .then((r) => r.json())
                .then((body) => {
                    if (body && typeof body === "object" && body.code === 0 && body.data) {
                        setDriverStatus(body.data as DriverStatus)
                    } else {
                        setDriverStatus(null)
                    }
                })
                .catch(() => setDriverStatus(null))
                .finally(() => setStatusLoading(false))
        }
    }, [open, defaultInstallPath])

    const filesAvailable = driverStatus?.files_available ?? false
    const iddCheckboxDisabled = statusLoading || !filesAvailable

    const onConfirm = async () => {
        setSubmitting(true)
        try {
            const resp = await fetch("/api/service/install", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({
                    install_path: installPath,
                    install_idd_driver: installIdd && filesAvailable,
                }),
            })
            const body = await resp.json().catch(() => null)
            const code = body?.code
            if (code === 0) {
                toast({
                    title: t("pages.system.settings.success"),
                    description: t(
                        "pages.system.settings.serviceManagement.installSuccess",
                    ),
                })
                onOpenChange(false)
            } else if (code === 5) {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error"),
                    description: t(
                        "pages.layout.serviceBanner.installDialog.invalidPath",
                    ),
                })
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error"),
                    description:
                        body?.message ??
                        t(
                            "pages.system.settings.serviceManagement.installError",
                        ),
                })
            }
        } catch (e) {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error"),
                description: t(
                    "pages.system.settings.serviceManagement.installError",
                ),
            })
        } finally {
            setSubmitting(false)
        }
    }

    return (
        <Dialog open={open} onOpenChange={onOpenChange}>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>{t("pages.layout.serviceBanner.title")}</DialogTitle>
                    <DialogDescription>
                        {t(
                            "pages.layout.serviceBanner.installDialog.description",
                        )}
                    </DialogDescription>
                </DialogHeader>
                <div className="space-y-4">
                    <div className="space-y-2">
                        <Label htmlFor="install-path">
                            {t("pages.layout.serviceBanner.installDialog.pathLabel")}
                        </Label>
                        <Input
                            id="install-path"
                            value={installPath}
                            onChange={(e) => setInstallPath(e.target.value)}
                        />
                    </div>
                    <div className="flex items-start gap-2">
                        <Checkbox
                            id="install-idd"
                            checked={installIdd && !iddCheckboxDisabled}
                            onCheckedChange={(v) => setInstallIdd(v === true)}
                            disabled={iddCheckboxDisabled}
                        />
                        <div className="grid gap-1.5 leading-none">
                            <Label htmlFor="install-idd" className="text-sm">
                                {t(
                                    "pages.layout.serviceBanner.installDialog.installIddDriver",
                                )}
                            </Label>
                            <p className="text-xs text-muted-foreground">
                                {statusLoading
                                    ? t(
                                          "pages.layout.serviceBanner.installDialog.iddDriverChecking",
                                      )
                                    : filesAvailable
                                      ? t(
                                            "pages.layout.serviceBanner.installDialog.iddDriverAvailable",
                                        )
                                      : t(
                                            "pages.layout.serviceBanner.installDialog.iddDriverFilesMissing",
                                        )}
                            </p>
                        </div>
                    </div>
                </div>
                <DialogFooter>
                    <Button
                        variant="outline"
                        onClick={() => onOpenChange(false)}
                        disabled={submitting}
                    >
                        {t("pages.layout.serviceBanner.installDialog.cancel")}
                    </Button>
                    <Button
                        onClick={onConfirm}
                        disabled={!installPath.trim() || submitting}
                    >
                        {t("pages.layout.serviceBanner.installButton")}
                    </Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}
