import type { RequestRemoteModel } from "@/services/types"

/**
 * Whether the desk config dialog should be (re)opened automatically.
 *
 * It opens for the initial settings pick (INIT arrived, no connect attempt yet)
 * and after a terminal ICE failure so the user can retry. It must not reopen on
 * a transient `disconnected`, because ICE can recover without user action.
 */
export function shouldOpenConfigDialog(args: {
    hasInitData: boolean
    isRTCConnected: boolean
    rtcFailed: boolean
    hasAttemptedConnect: boolean
}): boolean {
    if (!args.hasInitData || args.isRTCConnected) return false
    return !args.hasAttemptedConnect || args.rtcFailed
}

/**
 * The blocked-media warning owns the whole video surface. Hide it while the
 * settings dialog is open so the dialog and its portalled encoder picker are
 * not covered by the warning's higher stacking layer. The state is retained,
 * so cancelling the dialog shows the warning again.
 */
export function shouldShowMediaPipelineOverlay(
    hasPipelineState: boolean,
    isConfigOpen: boolean,
): boolean {
    return hasPipelineState && !isConfigOpen
}

type DesktopRequestRemotePayload = Pick<
    RequestRemoteModel,
    "purpose" | "grant_session_id"
> & {
    connection_id: string
}

export function buildDesktopRequestRemotePayload(
    connectionId: string,
    grantSessionId: string | null,
): DesktopRequestRemotePayload {
    return {
        connection_id: connectionId,
        purpose: "remote_desktop",
        ...(grantSessionId ? { grant_session_id: grantSessionId } : {}),
    }
}
