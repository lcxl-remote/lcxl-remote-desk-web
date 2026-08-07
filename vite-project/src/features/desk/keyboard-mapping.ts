import type { OperationSystemEnum } from "@/services/types"

type NavigatorPlatformInfo = Pick<Navigator, "platform" | "userAgent"> & {
    userAgentData?: {
        platform?: string
    }
}

export type DesktopControllerPlatform = "Windows" | "Linux"

/** Detect desktop controller platforms that need macOS-friendly shortcut mapping. */
export function getDesktopControllerPlatform(
    navigatorInfo: NavigatorPlatformInfo = navigator as NavigatorPlatformInfo,
): DesktopControllerPlatform | undefined {
    const platform = navigatorInfo.userAgentData?.platform
        || navigatorInfo.platform
        || navigatorInfo.userAgent
    if (/windows|win32|win64/i.test(platform)) {
        return "Windows"
    }
    if (!/android/i.test(navigatorInfo.userAgent) && /linux|x11/i.test(platform)) {
        return "Linux"
    }
    return undefined
}

/** Enable ergonomic desktop-keyboard mapping for Windows/Linux → macOS. */
export function getMacKeyboardMappingController(
    remoteOs: OperationSystemEnum | undefined,
    navigatorInfo?: NavigatorPlatformInfo,
): DesktopControllerPlatform | undefined {
    if (remoteOs !== "Mac") {
        return undefined
    }
    return getDesktopControllerPlatform(navigatorInfo)
}
