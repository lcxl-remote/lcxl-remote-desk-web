import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import { toast } from "@/hooks/use-toast"
import { Toaster } from "./toaster"

describe("Toaster", () => {
  it("wraps long unbroken error details inside the toast width", () => {
    const detail =
      'Custom desk error: {"error":{"message":"unknown_variant_image_url_expected_text"}}'
    toast({ title: "Connection test failed", description: detail })

    render(<Toaster />)

    const description = screen.getByText(detail)
    expect(description).toHaveClass("break-words", "[overflow-wrap:anywhere]")
    expect(description.parentElement).toHaveClass("min-w-0", "flex-1")
  })
})
