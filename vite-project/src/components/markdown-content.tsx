import ReactMarkdown, { type Components } from "react-markdown"
import remarkGfm from "remark-gfm"

const components: Components = {
    h1: ({ children }) => (
        <h1 className="mb-2 mt-3 text-base font-bold first:mt-0">{children}</h1>
    ),
    h2: ({ children }) => (
        <h2 className="mb-2 mt-3 text-sm font-bold first:mt-0">{children}</h2>
    ),
    h3: ({ children }) => (
        <h3 className="mb-1.5 mt-2.5 text-xs font-semibold first:mt-0">{children}</h3>
    ),
    p: ({ children }) => (
        <p className="mb-2 whitespace-pre-wrap last:mb-0">{children}</p>
    ),
    ul: ({ children }) => (
        <ul className="mb-2 list-disc space-y-1 pl-5 last:mb-0">{children}</ul>
    ),
    ol: ({ children }) => (
        <ol className="mb-2 list-decimal space-y-1 pl-5 last:mb-0">{children}</ol>
    ),
    li: ({ children }) => <li className="pl-0.5">{children}</li>,
    blockquote: ({ children }) => (
        <blockquote className="mb-2 border-l-2 border-white/30 pl-2 text-white/70 last:mb-0">
            {children}
        </blockquote>
    ),
    hr: () => <hr className="my-3 border-white/20" />,
    pre: ({ children }) => (
        <pre className="mb-2 overflow-x-auto whitespace-pre rounded bg-black/40 p-2 text-xs last:mb-0">
            {children}
        </pre>
    ),
    code: ({ children }) => (
        <code className="rounded bg-black/30 px-1 py-0.5 font-mono text-[0.9em] text-green-300">
            {children}
        </code>
    ),
    table: ({ children }) => (
        <div className="mb-2 overflow-x-auto rounded border border-white/15 last:mb-0">
            <table className="w-full border-collapse text-left text-xs">{children}</table>
        </div>
    ),
    thead: ({ children }) => <thead className="bg-white/10">{children}</thead>,
    th: ({ children }) => (
        <th className="border-b border-r border-white/15 px-2 py-1.5 font-semibold last:border-r-0">
            {children}
        </th>
    ),
    td: ({ children }) => (
        <td className="border-b border-r border-white/10 px-2 py-1.5 align-top last:border-r-0">
            {children}
        </td>
    ),
    a: ({ children, ...props }) => (
        <a
            {...props}
            target="_blank"
            rel="noopener noreferrer"
            className="text-blue-300 underline decoration-blue-300/50 underline-offset-2 hover:text-blue-200"
        >
            {children}
        </a>
    ),
    // Never fetch model-supplied image URLs; they can be used as tracking pixels.
    img: ({ alt }) => <span>{alt}</span>,
}

const componentsWithoutLinks: Components = {
    ...components,
    // Some model outputs contain source-like or configuration URLs. Keep their
    // visible label/text, but never make a model-controlled destination clickable.
    a: ({ children }) => <span>{children}</span>,
}

export function MarkdownContent({
    children,
    className = "",
    disableLinks = false,
}: {
    children: string
    className?: string
    disableLinks?: boolean
}) {
    return (
        <div className={`min-w-0 break-words ${className}`}>
            <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                components={disableLinks ? componentsWithoutLinks : components}
                skipHtml
            >
                {children}
            </ReactMarkdown>
        </div>
    )
}
