import { Disclosure } from '@/components/ui/disclosure';
import { Button } from '@/components/ui/button';
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { getAiAssistantContextSummary } from '@/services/clients';
import type { AiAssistantContextSummaryDto, ContextNoticeDto } from '@/services/types';
import type { AiAssistantMessage } from './use-ai-assistant-chat';

export function noticeMessageId(notice: ContextNoticeDto, messages: AiAssistantMessage[]): string | undefined {
    if (!notice.afterMessageId) return undefined;
    return messages.find(message => message.id === notice.afterMessageId
        || message.contextBoundaryIds?.includes(notice.afterMessageId!))?.id;
}

/** Jumps to a summarized original message; `available` hides jumps that cannot land. */
export type ContextSummarySourceNavigation = {
    available: (messageId: string) => boolean;
    open: (messageId: string) => void;
};

export function AssistantContextNotices({ notices, historical = false, sessionId, sources }: {
    notices: ContextNoticeDto[]; historical?: boolean; sessionId?: string; sources?: ContextSummarySourceNavigation;
}) {
    const { t, i18n } = useTranslation();
    if (!notices.length) return null;
    const rows = notices.map(notice => {
        const timestamp = notice.createdAt ? Date.parse(notice.createdAt) : NaN;
        const line = <p data-testid="assistant-context-notice" className="text-center text-xs text-muted-foreground">
            {Number.isFinite(timestamp)
                ? <time dateTime={notice.createdAt!}>{new Date(timestamp).toLocaleString(i18n.language)}</time>
                : t('pages.aiAssistant.contextNotice.unknownTime')}
            {' · '}
            {t(`pages.aiAssistant.contextNotice.${notice.kind}`)}
        </p>;
        return notice.kind === 'compacted' && sessionId && notice.checkpointGeneration != null
            ? <div key={notice.id} className="space-y-1">
                {line}
                <CompactionSummary sessionId={sessionId} turn={notice.turnId} generation={notice.checkpointGeneration} sources={sources} />
            </div>
            : <div key={notice.id}>{line}</div>;
    });
    if (historical) return <Disclosure className="text-xs text-muted-foreground" title={<>{t('pages.aiAssistant.contextNotice.earlier')}</>} summaryClassName="cursor-pointer">

        <div className="max-h-32 space-y-2 overflow-auto pt-2">{rows}</div>
    </Disclosure>;
    return <div className="space-y-2">{rows}</div>;
}

type SummaryView = { status: 'loading' } | { status: 'failed' } | { status: 'loaded'; data: AiAssistantContextSummaryDto };

/** Loads the summary only when the owner opens it. Facts are shown as plain text. */
function CompactionSummary({ sessionId, turn, generation, sources }: {
    sessionId: string; turn: string; generation: number; sources?: ContextSummarySourceNavigation;
}) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    const [view, setView] = useState<SummaryView>();
    const request = useRef(0);
    const load = () => {
        const id = ++request.current;
        setView({ status: 'loading' });
        getAiAssistantContextSummary({ session: sessionId, turn, generation })
            .then(body => {
                if (request.current !== id) return;
                setView(body.success && body.data ? { status: 'loaded', data: body.data } : { status: 'failed' });
            })
            .catch(() => { if (request.current === id) setView({ status: 'failed' }); });
    };
    useEffect(() => {
        request.current += 1;
        setView(undefined);
    }, [sessionId, turn, generation]);
    useEffect(() => {
        if (open && !view) load();
        // Loads once per opened notice; `load` changes every render.
    }, [open, view]);
    return <Disclosure open={open} onOpenChange={setOpen} className="mx-auto w-full max-w-[90%] text-xs text-muted-foreground"
        title={t(open ? 'pages.aiAssistant.contextSummary.hide' : 'pages.aiAssistant.contextSummary.show')} summaryClassName="cursor-pointer justify-center">
        <div data-testid="assistant-context-summary" className="mt-2 space-y-3 rounded-md border bg-muted/30 p-3 text-left">
            {view?.status === 'loading' && <p>{t('pages.aiAssistant.contextSummary.loading')}</p>}
            {view?.status === 'failed' && <p role="alert" className="flex items-center gap-2 text-destructive">
                {t('pages.aiAssistant.contextSummary.failed')}
                <Button type="button" size="sm" variant="ghost" onClick={load}>{t('pages.aiAssistant.contextSummary.retry')}</Button>
            </p>}
            {view?.status === 'loaded' && view.data.state !== 'available' && <p>{t(`pages.aiAssistant.contextSummary.${view.data.state}`)}</p>}
            {view?.status === 'loaded' && view.data.summary?.sections.map(section => <section key={section.kind} className="space-y-1">
                <h4 className="font-medium text-foreground">{t(`pages.aiAssistant.contextSummary.kind.${section.kind}`)}</h4>
                <ul className="list-disc space-y-1 pl-4">
                    {section.facts.map((fact, index) => {
                        const reachable = sources ? fact.sourceMessageIds.filter(sources.available) : [];
                        return <li key={index} className="whitespace-pre-wrap text-foreground">
                            {fact.text}
                            {reachable.map((messageId, sourceIndex) => <Button key={messageId} type="button" size="sm" variant="link"
                                className="h-auto px-1 py-0 text-xs" onClick={() => sources!.open(messageId)}>
                                {reachable.length > 1
                                    ? t('pages.aiAssistant.contextSummary.sourceNumbered', { index: sourceIndex + 1 })
                                    : t('pages.aiAssistant.contextSummary.source')}
                            </Button>)}
                        </li>;
                    })}
                </ul>
            </section>)}
        </div>
    </Disclosure>;
}
