import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"
import { BackendDiagnosticsCard } from "./backend-diagnostics-card"
import type { BackendInfo } from "@/services/types"

const linuxSection: BackendInfo["platform_diagnostics"][number] = {
    platform: "linux",
    key: "linux_display",
    items: [
        { key: "wayland_display", value: "true", status: "neutral", detail: null },
        { key: "screencast_portal", value: "unavailable", status: "error", detail: "service missing" },
    ],
}

describe("BackendDiagnosticsCard", () => {
    it("renders no platform section for an empty cross-platform payload", () => {
        const { container } = render(<BackendDiagnosticsCard sections={[]} />)
        expect(container).toBeEmptyDOMElement()
    })

    it("renders Linux items and error detail from the generic model", () => {
        render(<BackendDiagnosticsCard sections={[linuxSection]} />)
        expect(screen.getByText("WAYLAND_DISPLAY")).toBeInTheDocument()
        expect(screen.getByText("service missing")).toBeInTheDocument()
    })

    it("falls back to a readable label for future platform keys", () => {
        render(<BackendDiagnosticsCard sections={[{
            platform: "windows",
            key: "windows_capture",
            items: [{ key: "graphics_capture", value: "ready", status: "ready", detail: null }],
        }]} />)
        expect(screen.getByText("Windows Capture")).toBeInTheDocument()
        expect(screen.getByText("Graphics Capture")).toBeInTheDocument()
    })
})
