import { describe, expect, it } from "vitest"
import {
    DEFAULT_APPROVAL_TIMEOUT,
    mapTimeoutFromSelectValue,
    mapTimeoutToSelectValue,
} from "./security-timeout"

describe("mapTimeoutFromSelectValue", () => {
    it("maps 'never' (0) to the present value 0, not null", () => {
        const result = mapTimeoutFromSelectValue("0")
        expect(result).toBe(0)
        expect(result).not.toBeNull()
    })

    it("maps a positive selection to its numeric value", () => {
        expect(mapTimeoutFromSelectValue("30")).toBe(30)
        expect(mapTimeoutFromSelectValue("300")).toBe(300)
    })

    it("falls back to 0 for empty or invalid input", () => {
        expect(mapTimeoutFromSelectValue("")).toBe(0)
        expect(mapTimeoutFromSelectValue("abc")).toBe(0)
    })
})

describe("mapTimeoutToSelectValue", () => {
    it("renders a present 0 as 'never'", () => {
        expect(mapTimeoutToSelectValue(0)).toBe("0")
    })

    it("renders a missing value as the 30s default rather than 'never'", () => {
        expect(mapTimeoutToSelectValue(null)).toBe(DEFAULT_APPROVAL_TIMEOUT.toString())
        expect(mapTimeoutToSelectValue(undefined)).toBe(DEFAULT_APPROVAL_TIMEOUT.toString())
    })

    it("renders a configured value verbatim", () => {
        expect(mapTimeoutToSelectValue(60)).toBe("60")
    })
})

describe("never round-trips as a present zero", () => {
    it("select 'never' -> stored 0 -> displays 'never'", () => {
        const stored = mapTimeoutFromSelectValue("0")
        expect(stored).toBe(0)
        expect(mapTimeoutToSelectValue(stored)).toBe("0")
    })
})
