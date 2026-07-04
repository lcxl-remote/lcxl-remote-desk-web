import { describe, it, expect } from "vitest"

import {
    managerLinkReasonKey,
    MANAGER_LINK_QUOTA_EXCEEDED,
    MANAGER_LINK_MISSING_IDENTITY,
} from "./manager-link-reason"

describe("managerLinkReasonKey", () => {
    it("maps the quota-exceeded code to the quota-full key", () => {
        expect(managerLinkReasonKey(MANAGER_LINK_QUOTA_EXCEEDED)).toBe("pages.managerLink.quotaFull")
    })

    it("maps the missing-identity code to the missing-identity key", () => {
        expect(managerLinkReasonKey(MANAGER_LINK_MISSING_IDENTITY)).toBe("pages.managerLink.missingIdentity")
    })

    it("falls back to the generic key for an unknown code", () => {
        expect(managerLinkReasonKey(999)).toBe("pages.managerLink.genericBlocked")
    })

    it("falls back to the generic key when the code is null or undefined", () => {
        expect(managerLinkReasonKey(null)).toBe("pages.managerLink.genericBlocked")
        expect(managerLinkReasonKey(undefined)).toBe("pages.managerLink.genericBlocked")
    })
})
