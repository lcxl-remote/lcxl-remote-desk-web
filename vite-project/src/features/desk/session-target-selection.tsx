import { Monitor } from "lucide-react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"

export type SessionTargetDescriptor = {
    target_id: string
    display_name: string
    session_type?: string | null
    seat?: string | null
    foreground: boolean
    remote_desktop_ready: boolean
    terminal_ready: boolean
    file_ready: boolean
    assistant_ready: boolean
}

export type SessionTargetListData = {
    revision: number
    targets: SessionTargetDescriptor[]
}

export function parseSessionTargetList(value: unknown): SessionTargetListData | null {
    if (!value || typeof value !== "object") return null
    const candidate = value as Partial<SessionTargetListData>
    if (!Number.isSafeInteger(candidate.revision) || !Array.isArray(candidate.targets)) return null
    const targets = candidate.targets.filter((target): target is SessionTargetDescriptor => (
        !!target
        && typeof target === "object"
        && typeof target.target_id === "string"
        && typeof target.display_name === "string"
    ))
    return targets.length === candidate.targets.length
        ? { revision: candidate.revision!, targets }
        : null
}

type SessionTargetDialogProps = {
    targets: SessionTargetDescriptor[]
    onSelect: (targetId: string) => void
}

/**
 * Fail-closed chooser shown only after the host reports multiple capability-
 * ready session workers. There is deliberately no implicit foreground pick.
 */
export function SessionTargetDialog({ targets, onSelect }: SessionTargetDialogProps) {
    const { t } = useTranslation()

    return (
        <Dialog open={targets.length > 0}>
            <DialogContent
                className="sm:max-w-md"
                onEscapeKeyDown={(event) => event.preventDefault()}
                onInteractOutside={(event) => event.preventDefault()}
            >
                <DialogHeader>
                    <DialogTitle>{t("pages.sessionTarget.title")}</DialogTitle>
                    <DialogDescription>
                        {t("pages.sessionTarget.description")}
                    </DialogDescription>
                </DialogHeader>
                <div className="grid gap-2">
                    {targets.map((target) => (
                        <Button
                            key={target.target_id}
                            variant="outline"
                            className="h-auto justify-start gap-3 px-4 py-3 text-left"
                            onClick={() => onSelect(target.target_id)}
                        >
                            <Monitor className="h-5 w-5 shrink-0" />
                            <span className="min-w-0">
                                <span className="block truncate font-medium">
                                    {target.display_name}
                                </span>
                                <span className="block truncate text-xs text-muted-foreground">
                                    {[target.session_type, target.seat].filter(Boolean).join(" · ")
                                        || t("pages.sessionTarget.desktop")}
                                </span>
                            </span>
                        </Button>
                    ))}
                </div>
            </DialogContent>
        </Dialog>
    )
}
