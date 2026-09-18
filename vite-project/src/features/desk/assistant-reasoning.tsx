import { Disclosure } from '@/components/ui/disclosure';
import { useTranslation } from 'react-i18next';

export function AssistantReasoning({ text }: { text?: string | null }) {
    const { t } = useTranslation();
    if (!text?.trim()) return null;
    return <Disclosure className="mb-2 rounded-md border bg-muted/30 px-3 py-2 text-muted-foreground" title={<>{t('pages.aiAssistant.reasoning')}</>} summaryClassName="cursor-pointer select-none text-sm">

        <div className="mt-2 max-h-80 overflow-y-auto whitespace-pre-wrap break-words text-sm">{text}</div>
    </Disclosure>;
}
