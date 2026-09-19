import { useEffect, useMemo, useState } from 'react';
import { Check, Copy, WrapText } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';

function payload(text: string, format: 'auto' | 'text') {
    if (format === 'auto') {
        try {
            JSON.parse(text);
            // Format whitespace only: large integers, numeric notation, duplicate
            // keys and string escapes must retain their original representation.
            const tokens = text.match(/"(?:\\.|[^"\\])*"|[{}\[\],:]|[^\s{}\[\],:]+/g) ?? [];
            let depth = 0;
            const parts: string[] = [];
            const newline = () => '\n' + '  '.repeat(Math.min(depth, 64));
            tokens.forEach((token, index) => {
                if (token === '{' || token === '[') {
                    depth++;
                    parts.push(token + (tokens[index + 1] === (token === '{' ? '}' : ']') ? '' : newline()));
                } else if (token === '}' || token === ']') {
                    depth--;
                    parts.push((tokens[index - 1] === (token === '}' ? '{' : '[') ? '' : newline()) + token);
                } else parts.push(token === ',' ? ',' + newline() : token === ':' ? ': ' : token);
            });
            return { text: parts.join(''), json: true };
        }
        catch { /* Plain text and incomplete JSON remain readable verbatim. */ }
    }
    return { text, json: false };
}

function highlight(text: string) {
    // Bound DOM size for large results. React escapes every token as text.
    if (text.length > 64 * 1024) return text;
    const tokens = /("(?:\\.|[^"\\])*"\s*:|"(?:\\.|[^"\\])*"|\b(?:true|false|null)\b|-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b)/g;
    return text.split(tokens).map((token, index) => {
        const color = token.startsWith('"') ? (token.trimEnd().endsWith(':')
            ? 'text-sky-700 dark:text-sky-300' : 'text-emerald-700 dark:text-emerald-300')
            : /^(true|false|null)$/.test(token) ? 'text-violet-700 dark:text-violet-300'
                : /^-?\d/.test(token) ? 'text-amber-700 dark:text-amber-300' : undefined;
        return color ? <span key={index} className={color}>{token}</span> : token;
    });
}

export function AssistantCodeBlock({ text, label, format = 'auto', testId }: {
    text: string;
    label?: string;
    format?: 'auto' | 'text';
    testId?: string;
}) {
    const { t } = useTranslation();
    const [wrap, setWrap] = useState(false);
    const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
    const value = useMemo(() => payload(text, format), [text, format]);
    const content = useMemo(() => value.json ? highlight(value.text) : value.text, [value]);
    useEffect(() => {
        if (copyState === 'idle') return;
        const timer = setTimeout(() => setCopyState('idle'), 2000);
        return () => clearTimeout(timer);
    }, [copyState]);
    const copyLabel = t(`pages.aiAssistant.codeBlock.${copyState === 'copied' ? 'copied' : 'copy'}`);
    const wrapLabel = t('pages.aiAssistant.codeBlock.wrap');
    return <div className="min-w-0 overflow-hidden rounded-lg border border-border bg-muted/40 text-xs">
        <div className="flex min-w-0 items-center justify-between gap-2 border-b border-border bg-muted/60 px-3 py-1.5">
            <div className="flex min-w-0 items-center gap-2">
                {label && <span className="truncate font-medium" title={label}>{label}</span>}
                <span className="font-mono text-[10px] uppercase tracking-wide text-muted-foreground">{value.json ? 'JSON' : t('pages.aiAssistant.codeBlock.text')}</span>
            </div>
            <div className="flex shrink-0 items-center gap-1">
                <Button type="button" variant="ghost" size="icon" className="size-7" aria-label={wrapLabel} title={wrapLabel} aria-pressed={wrap} onClick={() => setWrap(!wrap)}><WrapText className="size-3.5" /></Button>
                <Button type="button" variant="ghost" size="icon" className="size-7" aria-label={copyLabel} title={copyLabel} onClick={async () => {
                    try { await navigator.clipboard.writeText(text); setCopyState('copied'); }
                    catch { setCopyState('failed'); }
                }}>{copyState === 'copied' ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}</Button>
            </div>
        </div>
        <pre data-testid={testId} tabIndex={0} aria-label={label} className={`assistant-scrollbar m-0 max-h-80 overflow-auto p-3 font-mono text-xs leading-relaxed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring ${wrap ? 'whitespace-pre-wrap [overflow-wrap:anywhere]' : 'whitespace-pre [overflow-wrap:normal]'}`}><code>{content}</code></pre>
        {copyState !== 'idle' && <p role="status" className="px-3 pb-2 text-muted-foreground">{t(`pages.aiAssistant.codeBlock.${copyState === 'copied' ? 'copied' : 'copyFailed'}`)}</p>}
    </div>;
}
