// Only these opaque markers become internal history links. Markdown URLs,
// code samples and existing links keep their existing rendering policy.
type Node = { type: string; value?: string; children?: Node[]; url?: string; data?: unknown };
export const assistantSourceIdPattern = /^src_[A-Za-z0-9_-]{22}$/;
// Answers cite sources by their conversation alias; older answers use native ids.
const assistantSourceAliasPattern = /^src[1-9][0-9]{0,13}$/;

export function assistantSourceHref(sourceId: string, revision?: number) {
    const query = new URLSearchParams({ source: sourceId });
    if (revision != null) query.set('revision', String(revision));
    return `/ai-assistant/conversation?${query}`;
}

export function remarkAssistantSources(options: { sources: readonly { source_id: string; source_revision: number; alias?: string | null }[]; sourceLabel: string; unavailableLabel: string }) {
    return (tree: Node) => {
        const visit = (node: Node) => {
            if (!node.children || ['code', 'inlineCode', 'link', 'linkReference'].includes(node.type)) return;
            node.children = node.children.flatMap(child => {
                if (child.type !== 'text' || !child.value) { visit(child); return [child]; }
                const output: Node[] = [];
                let offset = 0;
                for (const match of child.value.matchAll(/〔来源:([^〕\n]*)(?:〕|$)/g)) {
                    const start = match.index!;
                    if (start > offset) output.push({ type: 'text', value: child.value.slice(offset, start) });
                    const marker = match[1];
                    const valid = (source: { source_id: string; source_revision: number }) => assistantSourceIdPattern.test(source.source_id) && Number.isSafeInteger(source.source_revision) && source.source_revision > 0;
                    const index = assistantSourceAliasPattern.test(marker)
                        ? options.sources.findIndex(source => source.alias === marker && valid(source))
                        : assistantSourceIdPattern.test(marker) ? options.sources.findIndex(source => source.source_id === marker && valid(source)) : -1;
                    if (index >= 0 && match[0].endsWith('〕')) {
                        const source = options.sources[index];
                        output.push({ type: 'link', url: assistantSourceHref(source.source_id, source.source_revision),
                            data: { hProperties: { 'data-assistant-source': source.source_id, 'data-assistant-source-revision': source.source_revision } },
                            children: [{ type: 'text', value: `〔${options.sourceLabel} ${index + 1}〕` }] });
                    } else output.push({ type: 'text', value: `〔${options.unavailableLabel}〕` });
                    offset = start + match[0].length;
                }
                if (offset < child.value.length) output.push({ type: 'text', value: child.value.slice(offset) });
                return output;
            });
        };
        visit(tree);
    };
}
