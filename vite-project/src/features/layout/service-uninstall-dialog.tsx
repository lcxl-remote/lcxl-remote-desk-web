import * as React from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import { AsyncButton } from "@/components/async-button"
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"
import { useToast } from "@/hooks/use-toast"
import { serviceManagementErrorMessage, serviceOperationMessage } from "@/features/layout/service-management-error"

import { ServiceRequestError, useServiceOperation } from "./service-operation"

/** Shared native service uninstall confirmation and completion feedback. */
export interface ServiceUninstallDialogProps {
    open: boolean
    platform?: string
    onCompleted?: () => void
    onOpenChange: (open: boolean) => void
}

export function ServiceUninstallDialog(props: ServiceUninstallDialogProps) {
    const { open, onOpenChange, platform = "windows", onCompleted } = props
    const operation = useServiceOperation()
    const { t } = useTranslation()
    const { toast } = useToast()
    const [submitting, setSubmitting] = React.useState(false)
    const submittingRef = React.useRef(false)

    const onConfirm = async () => {
        if (submittingRef.current) return
        submittingRef.current = true
        setSubmitting(true)
        try {
            const result = await operation.run('uninstall')
            const completed = !result || ['succeeded', 'submitted'].includes(result.state)
            toast({
                variant: completed || result?.state === 'cancelled' ? 'default' : 'destructive',
                title: t(result?.state === 'succeeded' ? 'pages.system.settings.success' : 'pages.system.settings.serviceManagement.operationTitle'),
                description: serviceOperationMessage(t, 'uninstall', result),
            })
            onCompleted?.()
            if (completed) onOpenChange(false)
        } catch (e) {
            if (e instanceof DOMException && e.name === 'AbortError') return
            toast({
                variant: 'destructive', title: t('pages.system.settings.error'),
                description: e instanceof ServiceRequestError && e.busy
                    ? t('pages.system.settings.serviceManagement.busy')
                    : serviceManagementErrorMessage(t, e instanceof ServiceRequestError ? e.code : undefined,
                        e instanceof Error ? e.message : undefined, 'pages.system.settings.serviceManagement.uninstallError'),
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
                    <DialogTitle>
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.title",
                        )}
                    </DialogTitle>
                    <DialogDescription>
                        {t(
                            platform === "linux" ? "pages.system.settings.serviceManagement.linuxUninstallDescription" : "pages.system.settings.serviceManagement.uninstallDialog.description",
                        )}
                    </DialogDescription>
                </DialogHeader>
                {submitting && <p role="status" className="text-sm text-muted-foreground">{t(operation.disconnected
                    ? "pages.system.settings.serviceManagement.waitingDisconnected"
                    : "pages.system.settings.serviceManagement.waitingResult")}</p>}
                <DialogFooter>
                    <Button
                        variant="outline"
                        onClick={() => onOpenChange(false)}
                        disabled={submitting}
                    >
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.cancel",
                        )}
                    </Button>
                    <AsyncButton
                        variant="destructive"
                        pending={submitting}
                        pendingLabel={t("pages.system.settings.serviceManagement.uninstallDialog.uninstalling")}
                        onClick={onConfirm}
                    >
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.confirm",
                        )}
                    </AsyncButton>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}
