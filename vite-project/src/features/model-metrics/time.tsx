import { useTranslation } from 'react-i18next';
import { metricTime } from './format';

export function MetricTime({ value }: { value?: string | null }) {
    const { i18n } = useTranslation();
    return <time dateTime={value || undefined}>{metricTime(value, i18n.language)}</time>;
}
