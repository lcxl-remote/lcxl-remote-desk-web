import React from "react"
import {
    Breadcrumb,
    BreadcrumbItem,
    BreadcrumbLink,
    BreadcrumbList,
    BreadcrumbPage,
    BreadcrumbSeparator,
} from "@/components/ui/breadcrumb"
import { Separator } from "@/components/ui/separator"
import {
    SidebarInset,
    SidebarProvider,
    SidebarTrigger,
} from "@/components/ui/sidebar"
import { AppSidebar } from "./app-sidebar"
import { ModeToggle } from "@/components/mode-toggle"
import { LanguageToggle } from "@/components/language-toggle"
import { Outlet } from "react-router-dom"
import { Toaster } from "@/components/ui/toaster"
import { Button } from "@/components/ui/button"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import {
    Dialog,
    DialogContent,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useTranslation } from "react-i18next"
import { useQueryServerInfo } from "@/services/hooks/undefinedController/useQueryServerInfo"

function ServiceInstallBanner() {
    const { t } = useTranslation()
    const { data: serverInfoResp } = useQueryServerInfo()
    const serverInfo = serverInfoResp?.data
    const [dialogOpen, setDialogOpen] = React.useState(false)
    const [installPath, setInstallPath] = React.useState("")

    React.useEffect(() => {
        if (serverInfo?.default_install_path && !installPath) {
            setInstallPath(serverInfo.default_install_path)
        }
    }, [serverInfo?.default_install_path])

    // Show only in default (portable) mode when service is not installed
    if (!serverInfo || serverInfo.startup_mode !== "default" || serverInfo.service_installed !== false) {
        return null
    }

    // Server binary not found next to the current executable
    if (!serverInfo.server_binary_available) {
        return (
            <Alert variant="destructive" className="rounded-none border-x-0 border-t-0">
                <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                <AlertDescription>{t("pages.layout.serviceBanner.binaryNotFound")}</AlertDescription>
            </Alert>
        )
    }

    const handleConfirmInstall = () => {
        fetch("/api/service/install", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ install_path: installPath }),
        }).catch(console.error)
        setDialogOpen(false)
    }

    if (!serverInfo.is_admin) {
        return (
            <Alert variant="destructive" className="rounded-none border-x-0 border-t-0">
                <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                <AlertDescription>{t("pages.layout.serviceBanner.needsAdmin")}</AlertDescription>
            </Alert>
        )
    }

    return (
        <>
            <Alert className="rounded-none border-x-0 border-t-0 flex items-center justify-between gap-4">
                <div>
                    <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                    <AlertDescription>{t("pages.layout.serviceBanner.description")}</AlertDescription>
                </div>
                <Button size="sm" onClick={() => setDialogOpen(true)} className="shrink-0">
                    {t("pages.layout.serviceBanner.installButton")}
                </Button>
            </Alert>

            <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
                <DialogContent>
                    <DialogHeader>
                        <DialogTitle>{t("pages.layout.serviceBanner.title")}</DialogTitle>
                    </DialogHeader>
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
                    <DialogFooter>
                        <Button variant="outline" onClick={() => setDialogOpen(false)}>
                            {t("pages.layout.serviceBanner.installDialog.cancel")}
                        </Button>
                        <Button onClick={handleConfirmInstall} disabled={!installPath.trim()}>
                            {t("pages.layout.serviceBanner.installButton")}
                        </Button>
                    </DialogFooter>
                </DialogContent>
            </Dialog>
        </>
    )
}

export default function Layout() {
    return (
        <SidebarProvider>
            <AppSidebar />
            <SidebarInset>
                <header className="flex h-16 shrink-0 items-center justify-between gap-2 transition-[width,height] ease-linear group-has-[[data-collapsible=icon]]/sidebar-wrapper:h-12 border-b px-4">
                    <div className="flex items-center gap-2">
                        <SidebarTrigger className="-ml-1" />
                        <Separator orientation="vertical" className="mr-2 h-4" />
                        <Breadcrumb>
                            <BreadcrumbList>
                                <BreadcrumbItem>
                                    <BreadcrumbPage>Remote Desk</BreadcrumbPage>
                                </BreadcrumbItem>
                            </BreadcrumbList>
                        </Breadcrumb>
                    </div>
                    <div className="flex items-center gap-2">
                        <LanguageToggle />
                        <ModeToggle />
                    </div>
                </header>
                <ServiceInstallBanner />
                <div className="flex flex-1 flex-col overflow-hidden relative p-4 pt-0">
                    <div className="flex-1 relative rounded-xl bg-muted/50 overflow-hidden">
                        <Outlet />
                    </div>
                </div>
            </SidebarInset>
            <Toaster />
        </SidebarProvider>
    )
}
