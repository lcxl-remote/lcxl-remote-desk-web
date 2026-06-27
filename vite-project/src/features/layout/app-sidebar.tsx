
import * as React from "react"
import {
    Monitor,
    FolderOpen,
    Terminal,
    Settings,
    User,
    LogOut,
    ChevronRight,
    ChevronDown,
    Key,
    BarChart3,
} from "lucide-react"
import { useLocation, Link, useNavigate } from "react-router-dom"
import { useTranslation } from "react-i18next"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { useGetCurrentUser } from "@/services/hooks/userController/useGetCurrentUser"
import { useLogoutAccount } from "@/services/hooks/authController/useLogoutAccount"
import { Badge } from "@/components/ui/badge"

import {
    Sidebar,
    SidebarContent,
    SidebarFooter,
    SidebarHeader,
    SidebarMenu,
    SidebarMenuButton,
    SidebarMenuItem,
    SidebarMenuSub,
    SidebarMenuSubButton,
    SidebarMenuSubItem,
    SidebarRail,
    useSidebar,
} from "@/components/ui/sidebar"
import {
    Collapsible,
    CollapsibleContent,
    CollapsibleTrigger,
} from "@/components/ui/collapsible"
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuItem,
    DropdownMenuLabel,
    DropdownMenuSeparator,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Avatar, AvatarFallback, AvatarImage } from "@/components/ui/avatar"

export function AppSidebar({ ...props }: React.ComponentProps<typeof Sidebar>) {
    const { t } = useTranslation()
    const location = useLocation()
    const navigate = useNavigate()
    const { state } = useSidebar()

    const { data: serverInfoResp } = useQueryServerInfo()
    const { data: userResp, refetch: refetchUser } = useGetCurrentUser()
    const { mutateAsync: logout } = useLogoutAccount()

    const serverInfo = serverInfoResp?.data
    const user = userResp?.data

    const handleLogout = () => {
        logout().catch(console.error)
        navigate("/user/login", { replace: true })
    }

    const navItems: any[] = React.useMemo(() => {
        if (!user || !serverInfo) return [];

        const isDeviceUser = user.access === "device_user";

        let dynamicItems: any[] = [];

        if (isDeviceUser) {
            if (user.targetConnectionId) {
                dynamicItems.push({
                    title: "menu.desk_control",
                    url: `/desk/${user.targetConnectionId}/control`,
                    icon: Monitor,
                });
            }
        } else {
            if (serverInfo.startup_mode !== "desk_server") {
                dynamicItems.push({
                    title: "menu.desk",
                    url: "/desk/list",
                    icon: Monitor,
                });
            }

            if (serverInfo.startup_mode !== "desk_server") {
                dynamicItems.push({
                    title: "menu.usage",
                    url: "/usage",
                    icon: BarChart3,
                });
            }

            dynamicItems.push({
                title: "menu.settings",
                url: "/system",
                icon: Settings,
            });
        }

        return dynamicItems;

    }, [user, serverInfo]);

    const getModeLabel = () => {
        switch (serverInfo?.startup_mode) {
            case "signaling": return "Signaling";
            case "desk_server": return "Desk Server";
            case "default": return "Default";
            default: return "";
        }
    };

    return (
        <Sidebar collapsible="icon" {...props}>
            <SidebarHeader>
                <SidebarMenu>
                    <SidebarMenuItem>
                        <SidebarMenuButton size="lg" asChild>
                            <Link to="/">
                                <div className="flex aspect-square size-8 items-center justify-center rounded-lg overflow-hidden">
                                    <img src="/logo.svg" className="size-full" />
                                </div>
                                <div className="grid flex-1 text-left text-sm leading-tight">
                                    <span className="truncate font-semibold flex items-center gap-2">
                                        LCXL Remote
                                        {serverInfo && (
                                            <Badge variant="outline" className="text-[10px] px-1 py-0 h-4">
                                                {getModeLabel()}
                                            </Badge>
                                        )}
                                    </span>
                                    <span className="truncate text-xs">Web Console</span>
                                </div>
                            </Link>
                        </SidebarMenuButton>
                    </SidebarMenuItem>
                </SidebarMenu>
            </SidebarHeader>
            <SidebarContent>
                <SidebarMenu>
                    {navItems.map((item: any) => (
                        <React.Fragment key={item.title}>
                            {item.items ? (
                                <Collapsible
                                    key={item.title}
                                    asChild
                                    defaultOpen={true}
                                    className="group/collapsible"
                                >
                                    <SidebarMenuItem>
                                        <CollapsibleTrigger asChild>
                                            <SidebarMenuButton tooltip={t(item.title)}>
                                                <item.icon />
                                                <span>{t(item.title)}</span>
                                                <ChevronRight className="ml-auto transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                                            </SidebarMenuButton>
                                        </CollapsibleTrigger>
                                        <CollapsibleContent>
                                            <SidebarMenuSub>
                                                {item.items?.map((subItem: any) => (
                                                    <SidebarMenuSubItem key={subItem.title}>
                                                        <SidebarMenuSubButton asChild isActive={location.pathname === subItem.url}>
                                                            <Link to={subItem.url}>
                                                                <span>{t(subItem.title)}</span>
                                                            </Link>
                                                        </SidebarMenuSubButton>
                                                    </SidebarMenuSubItem>
                                                ))}
                                            </SidebarMenuSub>
                                        </CollapsibleContent>
                                    </SidebarMenuItem>
                                </Collapsible>
                            ) : (
                                <SidebarMenuItem key={item.title}>
                                    <SidebarMenuButton asChild tooltip={t(item.title)} isActive={location.pathname === item.url}>
                                        <Link to={item.url}>
                                            <item.icon />
                                            <span>{t(item.title)}</span>
                                        </Link>
                                    </SidebarMenuButton>
                                </SidebarMenuItem>
                            )}
                        </React.Fragment>
                    ))}
                </SidebarMenu>
            </SidebarContent>
            <SidebarFooter>
                <SidebarMenu>
                    <SidebarMenuItem>
                        <DropdownMenu>
                            <DropdownMenuTrigger asChild>
                                <SidebarMenuButton
                                    size="lg"
                                    className="data-[state=open]:bg-sidebar-accent data-[state=open]:text-sidebar-accent-foreground"
                                >
                                    <Avatar className="h-8 w-8 rounded-lg">
                                        <AvatarImage src="" alt="User" />
                                        <AvatarFallback className="rounded-lg">CN</AvatarFallback>
                                    </Avatar>
                                    <div className="grid flex-1 text-left text-sm leading-tight">
                                        <span className="truncate font-semibold">{user?.name || "User"}</span>
                                        <span className="truncate text-xs">{user?.access === "admin" ? t('pages.appSidebar.role.admin') : t('pages.appSidebar.role.deviceUser')}</span>
                                    </div>
                                    <ChevronDown className="ml-auto size-4" />
                                </SidebarMenuButton>
                            </DropdownMenuTrigger>
                            <DropdownMenuContent
                                className="w-[--radix-dropdown-menu-trigger-width] min-w-56 rounded-lg"
                                side="bottom"
                                align="end"
                                sideOffset={4}
                            >
                                <DropdownMenuLabel className="p-0 font-normal">
                                    <div className="flex items-center gap-2 px-1 py-1.5 text-left text-sm">
                                        <Avatar className="h-8 w-8 rounded-lg">
                                            <AvatarImage src="" alt="User" />
                                            <AvatarFallback className="rounded-lg">CN</AvatarFallback>
                                        </Avatar>
                                        <div className="grid flex-1 text-left text-sm leading-tight">
                                            <span className="truncate font-semibold">{user?.name || "User"}</span>
                                            <span className="truncate text-xs">{user?.access === "admin" ? t('pages.appSidebar.role.admin') : t('pages.appSidebar.role.deviceUser')}</span>
                                        </div>
                                    </div>
                                </DropdownMenuLabel>
                                <DropdownMenuSeparator />
                                {user?.access === "admin" && (
                                    <DropdownMenuItem asChild>
                                        <Link to="/user/settings" className="w-full cursor-pointer">
                                            <Settings className="mr-2 h-4 w-4" />
                                            {t('menu.account.settings')}
                                        </Link>
                                    </DropdownMenuItem>
                                )}
                                <DropdownMenuItem onClick={handleLogout} className="cursor-pointer">
                                    <LogOut className="mr-2 h-4 w-4" />
                                    {t('menu.account.logout')}
                                </DropdownMenuItem>
                            </DropdownMenuContent>
                        </DropdownMenu>
                    </SidebarMenuItem>
                </SidebarMenu>
            </SidebarFooter>
            <SidebarRail />
        </Sidebar>
    )
}
