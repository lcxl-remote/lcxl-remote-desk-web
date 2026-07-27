import { BarChart3, LifeBuoy, Monitor, Settings } from "lucide-react"
import type { LucideIcon } from "lucide-react"
import { startupModeEnum } from "@/services/types"

export type NavItem = {
    /** i18n key, resolved by the sidebar. */
    title: string
    url: string
    icon: LucideIcon
}

/** What the sidebar entries are derived from. */
export type NavContext = {
    /** `access` from the current-user query; `device_user` is a redeemed code. */
    access?: string | null
    /** The device a redeemed code is scoped to, when there is one. */
    targetConnectionId?: string | null
    /** `ServerInfo.startup_mode`, a bare string on the wire. */
    startupMode?: string | null
}

/**
 * The sidebar entries for one signed-in principal on one server.
 *
 * Kept apart from the component so the mode gating is directly testable: a
 * mistyped mode string silently offered pages the server does not serve.
 */
export function buildNavItems({
    access,
    targetConnectionId,
    startupMode,
}: NavContext): NavItem[] {
    // A redeemed device code is scoped to a single device: it gets that one
    // control entry and nothing else.
    if (access === "device_user") {
        return targetConnectionId
            ? [
                  {
                      title: "menu.desk_control",
                      url: `/desk/${targetConnectionId}/control`,
                      icon: Monitor,
                  },
              ]
            : []
    }

    // Compared against the generated enum rather than a literal so the
    // kebab-case spelling cannot drift from what the backend emits.
    const isDeskServer = startupMode === startupModeEnum["desk-server"]
    const items: NavItem[] = []

    if (!isDeskServer) {
        items.push({ title: "menu.desk", url: "/desk/list", icon: Monitor })
    }

    // Host-side "ask for remote help": a primary entry so non-technical users
    // can reach it directly. Available wherever this node can act as a host,
    // which is every mode except pure signaling.
    if (startupMode !== startupModeEnum.signaling) {
        items.push({ title: "menu.support", url: "/support", icon: LifeBuoy })
    }

    // The usage views read the local signal database, which a pure desk-server
    // does not have — its endpoints are not registered there, so the entry must
    // not be offered either.
    if (!isDeskServer) {
        items.push({ title: "menu.usage", url: "/usage", icon: BarChart3 })
    }

    items.push({ title: "menu.settings", url: "/system", icon: Settings })

    return items
}

/** The badge next to the product name; empty for modes that serve no console. */
export function startupModeLabel(startupMode?: string | null): string {
    switch (startupMode) {
        case startupModeEnum.signaling:
            return "Signaling"
        case startupModeEnum["desk-server"]:
            return "Desk Server"
        case startupModeEnum["service-daemon"]:
            return "Service Daemon"
        case startupModeEnum.default:
            return "Default"
        default:
            return ""
    }
}
