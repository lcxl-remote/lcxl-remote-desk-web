import { describe, it, expect } from "vitest"
import { connectionQuality } from "./connection-quality"

describe("connectionQuality", () => {
    it("reports good on low loss and low latency", () => {
        expect(connectionQuality(0, 30)).toBe("good")
        expect(connectionQuality(1, 100)).toBe("good")
        // A fresh connection with no measurement yet reads good, not poor.
        expect(connectionQuality(0, 0)).toBe("good")
    })

    it("reports fair on moderate loss or latency", () => {
        expect(connectionQuality(2, 100)).toBe("fair")
        expect(connectionQuality(0, 200)).toBe("fair")
        expect(connectionQuality(3, 249)).toBe("fair")
    })

    it("reports poor once loss or latency crosses the degrade point", () => {
        expect(connectionQuality(4, 100)).toBe("poor")
        expect(connectionQuality(0, 300)).toBe("poor")
        expect(connectionQuality(10, 500)).toBe("poor")
    })
})
