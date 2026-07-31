// Maps a manager-link rejection `error_code` to an i18n key describing why this
// host's registration is blocked. Kept as a pure function so the mapping is
// unit-testable without rendering the banner.
//
// Unknown codes resolve to a localized generic line rather than the backend
// message: the banner renders that message underneath as the detail, so falling
// back to it would print the same English text twice.

import { deskErrorCodeEnum } from "@/services/types"
import { deskErrorKeyOr, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"

const CODE_TO_KEY: ErrorCodeKeyMap = {
    // Device quota on the manager is full; the user must free a slot from a
    // control end before this host can register.
    [deskErrorCodeEnum.DEVICE_QUOTA_EXCEEDED]: "pages.managerLink.quotaFull",
    // This host has no device identity the manager can bind the registration to.
    [deskErrorCodeEnum.DEVICE_CLIENT_ID_REQUIRED]: "pages.managerLink.missingIdentity",
    [deskErrorCodeEnum.MANAGER_CREDENTIAL_REVOKED]: "pages.managerLink.credentialRevoked",
    [deskErrorCodeEnum.MANAGER_CREDENTIAL_SUSPENDED]: "pages.managerLink.credentialSuspended",
}

export function managerLinkReasonKey(errorCode: number | null | undefined): string {
    return deskErrorKeyOr(CODE_TO_KEY, errorCode, "pages.managerLink.genericBlocked")
}

export function managerLinkTitleKey(errorCode: number | null | undefined): string {
    if (errorCode === deskErrorCodeEnum.MANAGER_CREDENTIAL_REVOKED) {
        return "pages.managerLink.credentialRevokedTitle"
    }
    if (errorCode === deskErrorCodeEnum.MANAGER_CREDENTIAL_SUSPENDED) {
        return "pages.managerLink.credentialSuspendedTitle"
    }
    return "pages.managerLink.blockedTitle"
}
