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
import { deskErrorCodeEnum } from "@/services/types"

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
            if (body?.code === deskErrorCodeEnum.SUCCESS) {
                toast({
                    title: t("pages.system.settings.success"),
                    description: t(
                        "pages.system.settings.serviceManagement.uninstallSuccess",
                    ),
                })
                onOpenChange(false)
            } else {
                toast({
                    variant: "destructive",
                    title: t("pages.system.settings.error"),
                    description:
                        body?.message ??
                        t(
                            "pages.system.settings.serviceManagement.uninstallError",
                        ),
                })
            }
        } catch (e) {
            toast({
                variant: "destructive",
                title: t("pages.system.settings.error"),
                description: t(
                    "pages.system.settings.serviceManagement.uninstallError",
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
                        )}
                    </DialogTitle>
                    <DialogDescription>
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.description",
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
                        )}
                    </Button>
                    <Button
                        variant="destructive"
                        onClick={onConfirm}
                        disabled={submitting}
                    >
                        {t(
                            "pages.system.settings.serviceManagement.uninstallDialog.confirm",
                        )}
                    </Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    )
}
