import { Sparkles } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { cn } from '@/lib/utils'

/** Frontend mirror of the Rust `AiProvenance` wire type (agent-protocol). */
export type AiProvenance = {
    model_id?: string | null
    generated_at?: string | null
    marking_scheme?: string | null
}

/**
 * Visible "AI-generated" marking for AI-produced content (EU AI Act Art.50(2)).
 * Render it wherever AI-generated text is shown.
 *
 * The marking is driven by the content being AI-generated — not by `provenance`
 * being present — so callers render it whenever they show AI output, and a
 * missing or stripped provenance never hides it (fail-closed). Provenance only
 * enriches the tooltip with the model that produced the content.
 *
 * `className` overrides the default themed styling for contexts that need it
 * (e.g. a fixed dark assistant overlay).
 */
export function AiGeneratedMark({
    provenance,
    className,
}: {
    provenance?: AiProvenance | null
    className?: string
}) {
    const { t } = useTranslation()
    const model = provenance?.model_id?.trim()
    const tooltip = model
        ? t('component.aiGenerated.byModel', { model })
        : t('component.aiGenerated.label')
    return (
        <span
            role="note"
            aria-label={t('component.aiGenerated.ariaLabel')}
            title={tooltip}
            className={cn(
                'inline-flex items-center gap-1 rounded border border-border bg-muted/40 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground',
                className,
            )}
        >
            <Sparkles className="h-3 w-3" aria-hidden="true" />
            {t('component.aiGenerated.label')}
        </span>
    )
}
