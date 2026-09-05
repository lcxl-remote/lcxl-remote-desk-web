import { useTranslation } from 'react-i18next';
import type { ContextNoticeDto } from '@/services/types';
import type { DeviceAssistantMessage } from './use-device-assistant-chat';

export function noticeMessageId(notice: ContextNoticeDto, messages: DeviceAssistantMessage[]): string | undefined {
    if (!notice.afterMessageId) return undefined;
    return messages.find(message => message.id === notice.afterMessageId
        || message.contextBoundaryIds?.includes(notice.afterMessageId!))?.id;
}

export function AssistantContextNotices({ notices, historical = false }: { notices: ContextNoticeDto[]; historical?: boolean }) {
    const { t, i18n } = useTranslation();
    if (!notices.length) return null;
    const rows = notices.map(notice => {
        const timestamp = notice.createdAt ? Date.parse(notice.createdAt) : NaN;
        return <p key={notice.id} data-testid="assistant-context-notice" className="text-center text-xs text-muted-foreground">
            {Number.isFinite(timestamp)
                ? <time dateTime={notice.createdAt!}>{new Date(timestamp).toLocaleString(i18n.language)}</time>
                : t('pages.deviceAssistant.contextNotice.unknownTime')}
            {' · '}
            {t(notice.kind === 'compacted' ? 'pages.deviceAssistant.contextNotice.compacted' : 'pages.deviceAssistant.contextNotice.trimmed')}
        </p>;
    });
    if (historical) return <details className="text-xs text-muted-foreground">
        <summary className="cursor-pointer">{t('pages.deviceAssistant.contextNotice.earlier')}</summary>
        <div className="max-h-32 space-y-2 overflow-auto pt-2">{rows}</div>
    </details>;
    return <div className="space-y-2">{rows}</div>;
}
