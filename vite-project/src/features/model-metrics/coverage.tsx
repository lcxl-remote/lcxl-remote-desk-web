import { useTranslation } from 'react-i18next';
import { metricTime } from './format';
import { MetricTime } from './time';
import type { QueryCoverage } from '@/services/types';

export function MetricsCoverage({ value, empty = false }: { value: QueryCoverage; empty?: boolean }) {
    const { t, i18n } = useTranslation();
    return <div role="status" className="space-y-2 text-sm">
        <p className="text-muted-foreground"><MetricTime value={value.effective_from}/> — <MetricTime value={value.effective_to}/> · {t(`modelMetrics.state.${value.sample_status}`)} · {t('modelMetrics.asOf')}: <MetricTime value={value.as_of}/></p>
        {value.not_collected_before && <p>{t('modelMetrics.beforeCapture', { time: metricTime(value.not_collected_before, i18n.language) })}</p>}
        {value.trimmed_before && <p>{t('modelMetrics.storage.trimmedHint', { time: metricTime(value.trimmed_before, i18n.language) })}</p>}
        {value.sample_status === 'not_collected' ? <p>{t('modelMetrics.empty.notCollected')}</p>
            : value.sample_status === 'unavailable' ? <p>{t('modelMetrics.empty.unavailable')}</p>
            : empty || value.sample_status === 'no_samples' ? <p>{t('modelMetrics.empty.noSamples')}</p> : null}
        {value.sample_status === 'partial' && <p>{t('modelMetrics.empty.partial')}</p>}
    </div>;
}
