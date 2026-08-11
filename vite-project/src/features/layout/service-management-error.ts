import type { TFunction } from "i18next"

import { deskErrorMessage, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"
import { deskErrorCodeEnum } from "@/services/types"

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
