import React from "react"
import {
    Breadcrumb,
    BreadcrumbItem,
    BreadcrumbList,
    BreadcrumbPage,
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
import { Button } from "@/components/ui/button"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useTranslation } from "react-i18next"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import RequireAuth from "@/features/auth/require-auth"
import { ServiceInstallDialog } from "./service-install-dialog"

function ServiceInstallBanner() {
    const { t } = useTranslation()
    const { data: serverInfoResp } = useQueryServerInfo()
    const serverInfo = serverInfoResp?.data
    const [dialogOpen, setDialogOpen] = React.useState(false)

    // macOS has no OS-service / UAC concept (background auto-start is a
    // LaunchAgent). The `background_start` field is only present on macOS,
    // so its presence is the platform signal for hiding this banner.
    if (serverInfo?.background_start != null) {
        return null
    }

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

            <ServiceInstallDialog
                open={dialogOpen}
                onOpenChange={setDialogOpen}
                defaultInstallPath={serverInfo.default_install_path ?? ""}
            />
        </>
    )
}

function Layout() {
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
        </SidebarProvider>
    )
}

export function AuthenticatedLayout() {
    return (
        <RequireAuth>
            <Layout />
        </RequireAuth>
    )
}
