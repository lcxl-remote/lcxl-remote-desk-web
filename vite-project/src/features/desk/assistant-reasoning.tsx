import { useTranslation } from 'react-i18next';

export function AssistantReasoning({ text }: { text?: string | null }) {
    const { t } = useTranslation();
    if (!text?.trim()) return null;
    return <details className="mb-2 rounded-md border bg-muted/30 px-3 py-2 text-muted-foreground">
        <summary className="cursor-pointer select-none text-sm">{t('pages.deviceAssistant.reasoning')}</summary>
        <div className="mt-2 max-h-80 overflow-y-auto whitespace-pre-wrap break-words text-sm">{text}</div>
    </details>;
}
