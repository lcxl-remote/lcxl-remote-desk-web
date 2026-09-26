import { useTranslation } from 'react-i18next';
import type { AiAssistantVisualEvidence } from './ai-assistant-event';

export function AssistantFrameTiming({ frame }: { frame: AiAssistantVisualEvidence['frame_observation'] }) {
    const { t } = useTranslation();
    if (!frame) return null;
    const valid = Number.isSafeInteger(frame.received_at_unix_ms) && frame.received_at_unix_ms > 0
        && Number.isSafeInteger(frame.receipt_age_ms) && frame.receipt_age_ms >= 0
        && (frame.source_timestamp_ns == null || (Number.isInteger(frame.source_timestamp_ns) && frame.source_timestamp_ns >= 0))
        && ['latest_observed', 'fresh', 'unchanged_verified'].includes(frame.freshness);
    const status = !valid ? 'unavailable' : frame.freshness === 'latest_observed' ? 'latestObserved'
        : frame.freshness === 'fresh' ? 'freshFrame'
            : frame.freshness === 'unchanged_verified' ? 'unchangedFrame' : 'unavailable';
    return <div className="space-y-1 px-2 py-1 text-xs text-muted-foreground">
        <p>{t(`pages.aiAssistant.observation.${status}`)}</p>
        {valid && <p>
            {t('pages.aiAssistant.observation.frameAge')}: {t('pages.aiAssistant.observation.frameAgeValue', { age: frame.receipt_age_ms })}
        </p>}
        {frame.source_timestamp_ns == null && <p>{t('pages.aiAssistant.observation.sourceTimeUnavailable')}</p>}
        <p>{t('pages.aiAssistant.observation.historicalFrame')}</p>
    </div>;
}
