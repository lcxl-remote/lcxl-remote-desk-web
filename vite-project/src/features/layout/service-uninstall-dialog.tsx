import * as React from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"
import { useToast } from "@/hooks/use-toast"

/**
 * Shared "uninstall service" confirmation dialog. Uninstalling the
 * service always force-uninstalls the LcxlVirtualDisplay IDD driver
 * (Q7) — the dialog body explicitly calls this out so the user
 * understands the side effect.
 */
export interface ServiceUninstallDialogProps {
    open: boolean
    onOpenChange: (open: boolean) => void
}

export function ServiceUninstallDialog(props: ServiceUninstallDialogProps) {
    const { open, onOpenChange } = props
    const { t } = useTranslation()
    const { toast } = useToast()
    const [submitting, setSubmitting] = React.useState(false)

    const onConfirm = async () => {
        setSubmitting(true)
        try {
            const resp = await fetch("/api/service/uninstall", { method: "POST" })
            const body = await resp.json().catch(() => null)
            if (body?.code === 0) {
                toast({
                    title: t("pages.system.settings.success", "Success"),
                    description: t(
                        "pages.system.settings.serviceManagement.uninstallSuccess",
                        "Service uninstall request submitted. Please wait a few seconds.",
                    ),
                })
                onOpenChange(false)
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error", "Error"),
                    description:
                        body?.message ??
                        t(
                            "pages.system.settings.serviceManagement.uninstallError",
                            "Failed to uninstall service.",
                        ),
                })
            }
        } catch (e) {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error", "Error"),
                description: t(
                    "pages.system.settings.serviceManagement.uninstallError",
                    "Failed to uninstall service.",
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
                    <DialogTitle>
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.title",
                            "Uninstall service?",
                        )}
                    </DialogTitle>
                    <DialogDescription>
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.description",
                            "This will stop the service and remove every installed copy of the LcxlVirtualDisplay IDD driver.",
                        )}
                    </DialogDescription>
                </DialogHeader>
                <DialogFooter>
                    <Button
                        variant="outline"
                        onClick={() => onOpenChange(false)}
                        disabled={submitting}
                    >
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.cancel",
                            "Cancel",
                        )}
                    </Button>
                    <Button
                        variant="destructive"
                        onClick={onConfirm}
                        disabled={submitting}
                    >
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.confirm",
                            "Uninstall",
                        )}
                    </Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}
