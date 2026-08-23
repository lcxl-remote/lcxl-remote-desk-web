import { render, screen } from "@testing-library/react"
import { describe, expect, it } from "vitest"
import { MarkdownContent } from "./markdown-content"

describe("MarkdownContent link policy", () => {
    it("keeps links clickable by default for existing consumers", () => {
        render(<MarkdownContent>{"[Guide](https://example.test)"}</MarkdownContent>)

        expect(screen.getByRole("link", { name: "Guide" })).toHaveAttribute(
            "href",
            "https://example.test",
        )
    })

    it("renders markdown links and bare URLs as non-clickable text when disabled", () => {
        render(
            <MarkdownContent disableLinks>
                {"[Guide](https://example.test) and https://manager.example.test"}
            </MarkdownContent>,
        )

        expect(screen.queryByRole("link")).toBeNull()
        expect(screen.getByText("Guide")).toBeInTheDocument()
        expect(screen.getByText("https://manager.example.test")).toBeInTheDocument()
    })
})
