import * as React from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import { AsyncButton } from "@/components/async-button"
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
import { deskErrorCodeEnum } from "@/services/types"
import { serviceManagementErrorMessage, serviceOperationMessage } from "@/features/layout/service-management-error"
import { ServiceRequestError, useServiceOperation } from "./service-operation"

/**
 * Shared "install service" confirmation dialog. Used by both the
 * layout banner and the system-settings page so the two entry points
 * present an identical UX (the system-settings page previously skipped
 * the confirmation step entirely).
 *
 * On Windows the dialog fetches `/api/virtual-display/driver/status` so
 * the "also install IDD virtual display driver" checkbox can be
 * disabled (with an inline hint) when the driver files are not
 * present next to the server binary.
 */
export interface ServiceInstallDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
    defaultInstallPath: string
    platform?: string
    onCompleted?: () => void
}

interface DriverStatus {
    files_available: boolean
    files_dir: string | null
    installed: boolean | null
    installed_oem_infs: string[] | null
    can_modify: boolean
}

export function ServiceInstallDialog(props: ServiceInstallDialogProps) {
    const { open, onOpenChange, defaultInstallPath, platform = "windows", onCompleted } = props
    const isLinux = platform === "linux"
    const operation = useServiceOperation()
    const { t } = useTranslation()
    const { toast } = useToast()

    const [installPath, setInstallPath] = React.useState(defaultInstallPath)
    const [installIdd, setInstallIdd] = React.useState(false)
    const [submitting, setSubmitting] = React.useState(false)
    const submittingRef = React.useRef(false)
    const [driverStatus, setDriverStatus] = React.useState<DriverStatus | null>(null)
    const [statusLoading, setStatusLoading] = React.useState(false)

    React.useEffect(() => {
        if (open) {
            setInstallPath(defaultInstallPath)
            setInstallIdd(false)
            if (isLinux) { setDriverStatus(null); setStatusLoading(false); return }
            setStatusLoading(true)
            fetch("/api/virtual-display/driver/status")
                .then((r) => r.json())
                .then((body) => {
                    if (body && typeof body === "object" && body.code === deskErrorCodeEnum.SUCCESS && body.data) {
                        setDriverStatus(body.data as DriverStatus)
                    } else {
                        setDriverStatus(null)
                    }
                })
                .catch(() => setDriverStatus(null))
                .finally(() => setStatusLoading(false))
        }
    }, [open, defaultInstallPath, isLinux])

    const filesAvailable = driverStatus?.files_available ?? false
    const iddCheckboxDisabled = statusLoading || !filesAvailable

    const onConfirm = async () => {
        if (submittingRef.current) return
        submittingRef.current = true
        setSubmitting(true)
        try {
            const result = await operation.run('install', {
                install_path: installPath,
                install_idd_driver: !isLinux && installIdd && filesAvailable,
            })
            const completed = !result || ['succeeded', 'submitted'].includes(result.state)
            toast({
                variant: completed || result?.state === 'cancelled' ? 'default' : 'destructive',
                title: t(result?.state === 'succeeded' ? 'pages.system.settings.success' : 'pages.system.settings.serviceManagement.operationTitle'),
                description: serviceOperationMessage(t, 'install', result),
            })
            onCompleted?.()
            if (completed) onOpenChange(false)
        } catch (e) {
            if (e instanceof DOMException && e.name === 'AbortError') return
            toast({
                variant: 'destructive', title: t('pages.system.settings.error'),
                description: e instanceof ServiceRequestError && e.busy
                    ? t('pages.system.settings.serviceManagement.busy')
                    : e instanceof ServiceRequestError && e.code === deskErrorCodeEnum.INVALID_PARAMS
                        ? t('pages.layout.serviceBanner.installDialog.invalidPath')
                        : serviceManagementErrorMessage(t, e instanceof ServiceRequestError ? e.code : undefined,
                            e instanceof Error ? e.message : undefined, 'pages.system.settings.serviceManagement.installError'),
            })
        } finally {
            submittingRef.current = false
            setSubmitting(false)
        }
    }

    return (
        <Dialog open={open} onOpenChange={(nextOpen) => !submitting && onOpenChange(nextOpen)}>
            <DialogContent>
                <DialogHeader>
                    <DialogTitle>{t(isLinux ? "pages.system.settings.serviceManagement.linuxTitle" : "pages.layout.serviceBanner.title")}</DialogTitle>
                    <DialogDescription>
                        {t(
                            isLinux ? "pages.system.settings.serviceManagement.linuxInstallDescription" : "pages.layout.serviceBanner.installDialog.description",
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
                            readOnly={isLinux}
                            disabled={submitting}
                            onChange={(e) => setInstallPath(e.target.value)}
                        />
                    </div>
                    {!isLinux && <div className="flex items-start gap-2">
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
                    </div>}
                    {submitting && <p role="status" className="text-sm text-muted-foreground">{t(operation.disconnected
                        ? "pages.system.settings.serviceManagement.waitingDisconnected"
                        : "pages.system.settings.serviceManagement.waitingResult")}</p>}
                </div>
                <DialogFooter>
                    <Button
                        variant="outline"
                        onClick={() => onOpenChange(false)}
                        disabled={submitting}
                    >
                        {t("pages.layout.serviceBanner.installDialog.cancel")}
                    </Button>
                    <AsyncButton
                        pending={submitting}
                        pendingLabel={t("pages.layout.serviceBanner.installDialog.installing")}
                        onClick={onConfirm}
                        disabled={!installPath.trim()}
                    >
                        {t("pages.layout.serviceBanner.installButton")}
                    </AsyncButton>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}
