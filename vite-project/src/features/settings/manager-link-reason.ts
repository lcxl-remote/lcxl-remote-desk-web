// Maps a manager-link rejection `error_code` (a `DeskErrorCode`) to an i18n key
// describing why this host's registration is blocked. Kept as a pure function so
// the mapping is unit-testable without rendering the banner.

// Device quota on the manager is full; the user must free a slot from a control
// end before this host can register.
export const MANAGER_LINK_QUOTA_EXCEEDED = 46
// This host has no device identity the manager can bind the registration to.
export const MANAGER_LINK_MISSING_IDENTITY = 47

export function managerLinkReasonKey(errorCode: number | null | undefined): string {
    switch (errorCode) {
        case MANAGER_LINK_QUOTA_EXCEEDED:
            return "pages.managerLink.quotaFull"
        case MANAGER_LINK_MISSING_IDENTITY:
            return "pages.managerLink.missingIdentity"
        default:
            return "pages.managerLink.genericBlocked"
    }
}
