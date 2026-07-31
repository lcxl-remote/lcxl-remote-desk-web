import { describe, it, expect } from "vitest"

import { deskErrorCodeEnum } from "@/services/types"
import { managerLinkReasonKey, managerLinkTitleKey } from "./manager-link-reason"

describe("managerLinkReasonKey", () => {
    it("maps the quota-exceeded code to the quota-full key", () => {
        expect(managerLinkReasonKey(deskErrorCodeEnum.DEVICE_QUOTA_EXCEEDED)).toBe("pages.managerLink.quotaFull")
    })

    it("maps the missing-identity code to the missing-identity key", () => {
        expect(managerLinkReasonKey(deskErrorCodeEnum.DEVICE_CLIENT_ID_REQUIRED)).toBe("pages.managerLink.missingIdentity")
    })

    it("distinguishes revoked and recoverable manager credentials", () => {
        expect(managerLinkReasonKey(deskErrorCodeEnum.MANAGER_CREDENTIAL_REVOKED)).toBe(
            "pages.managerLink.credentialRevoked",
        )
        expect(managerLinkTitleKey(deskErrorCodeEnum.MANAGER_CREDENTIAL_REVOKED)).toBe(
            "pages.managerLink.credentialRevokedTitle",
        )
        expect(managerLinkReasonKey(deskErrorCodeEnum.MANAGER_CREDENTIAL_SUSPENDED)).toBe(
            "pages.managerLink.credentialSuspended",
        )
        expect(managerLinkTitleKey(deskErrorCodeEnum.MANAGER_CREDENTIAL_SUSPENDED)).toBe(
            "pages.managerLink.credentialSuspendedTitle",
        )
    })

    it("falls back to the generic key for an unknown code", () => {
        expect(managerLinkReasonKey(999)).toBe("pages.managerLink.genericBlocked")
    })

    it("falls back to the generic key when the code is null or undefined", () => {
        expect(managerLinkReasonKey(null)).toBe("pages.managerLink.genericBlocked")
        expect(managerLinkReasonKey(undefined)).toBe("pages.managerLink.genericBlocked")
    })

    // The banner shows a generic headline plus the backend message as detail, so
    // the fallback must stay a localized key. Returning the backend message here
    // would print it twice, in English, as the headline.
    it("never returns the backend message as the headline", () => {
        expect(managerLinkReasonKey(999).startsWith("pages.managerLink.")).toBe(true)
    })
})
