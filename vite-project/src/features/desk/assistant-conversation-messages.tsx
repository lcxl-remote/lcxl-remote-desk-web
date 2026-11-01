import type { ComponentProps, ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { AssistantTranscript } from './assistant-transcript';

/** Shared conversation presentation; paging and input authority remain with the caller. */
export function AssistantConversationMessages({ loading, hasMore, loadingOlder, onOlder, before, children, ...transcript }: ComponentProps<typeof AssistantTranscript> & {
    loading?: boolean; hasMore?: boolean; loadingOlder?: boolean; onOlder?: () => void;
    before?: ReactNode; children?: ReactNode;
}) {
    const { t } = useTranslation();
    return <div data-testid="ai-assistant-transcript" className="min-h-48 space-y-5">
        {loading && <Skeleton className="h-20 w-full" />}
        {hasMore && <div className="flex justify-center">
            <Button type="button" size="sm" variant="ghost" disabled={loadingOlder} onClick={onOlder}>
                {loadingOlder && <LoaderCircle className="mr-2 h-4 w-4 animate-spin" />}
                {t('pages.aiAssistant.loadEarlierMessages')}
            </Button>
        </div>}
        {before}
        <AssistantTranscript {...transcript} />
        {children}
    </div>;
}
