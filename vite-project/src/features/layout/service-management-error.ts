import type { TFunction } from "i18next"

import { deskErrorMessage, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"
import { deskErrorCodeEnum } from "@/services/types"
import type { ServiceOperationStatus } from "@/services/types"

const SERVICE_MANAGEMENT_ERROR_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.PRECONDITION_FAILED]: "pages.system.settings.serviceManagement.initializeFirst",
    [deskErrorCodeEnum.PERMISSION_ERROR]: "pages.system.settings.serviceManagement.ownerRequired",
}

export function serviceManagementErrorMessage(
    t: TFunction,
    code: number | null | undefined,
    message: string | null | undefined,
    fallbackKey: string,
): string {
    return deskErrorMessage(t, SERVICE_MANAGEMENT_ERROR_KEYS, code, message, t(fallbackKey))
}

export function serviceOperationMessage(t: TFunction, op: 'install' | 'uninstall', result?: ServiceOperationStatus): string {
    const root = 'pages.system.settings.serviceManagement'
    if (!result || result.state === 'submitted') return t(`${root}.${op}Success`)
    if (result.state === 'succeeded') return t(`${root}.${op}Completed`)
    if (result.state === 'cancelled') return t(`${root}.authorizationCancelled`)
    if (result.state === 'unknown') return t(`${root}.resultUnknown`)
    const errors = {
        authorization_not_granted: `${root}.authorizationNotGranted`,
        missing_pkexec: `${root}.missingPkexec`,
        launch_failed: `${root}.launchFailed`,
        installer_failed: `${root}.installerFailed`,
        busy: `${root}.busy`,
        unsupported: `${root}.unsupported`,
        connection_lost: `${root}.resultUnknown`,
        timed_out: `${root}.resultUnknown`,
    }
    return t(result.error ? errors[result.error] : `${root}.${op}Error`, { code: result.exit_code ?? '?' })
}
