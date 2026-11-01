import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import type { AssistantProcessingStatus } from '@/services/types';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';
import { assistantDataErrorKey, getAssistantProcessing } from './assistant-data-api';
const prefix = 'pages.aiAssistant.processing.';
export default function AssistantProcessing() {
    const { t } = useTranslation();
    const [value, setValue] = useState<AssistantProcessingStatus | null>(null);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const epoch = useRef(0);
    const load = async () => {
        const stamp = ++epoch.current; setLoading(true); setError('');
        try { const next = await getAssistantProcessing(); if (stamp === epoch.current) setValue(next); }
        catch (cause) { if (stamp === epoch.current) { setValue(null); setError(assistantDataErrorKey(cause)); } }
        finally { if (stamp === epoch.current) setLoading(false); }
    };
    useEffect(() => { void load(); return () => { epoch.current++; }; }, []);
    return <AiAssistantHubLayout>
        <div className="flex items-center justify-between gap-3"><h1 className="text-2xl font-semibold">{t(`${prefix}title`)}</h1>
            <Button variant="outline" disabled={loading} onClick={() => void load()}>{t('pages.aiAssistant.memory.refresh')}</Button></div>
        {loading && <p role="status">{t('pages.aiAssistant.memory.loading')}</p>}
        {error && <p role="alert" className="text-destructive">{t(error)}</p>}
        {value && <>
            {value.partial && <p>{t(`${prefix}partial`)}</p>}
            <Card><CardHeader><CardTitle>{t(`${prefix}history`)}</CardTitle></CardHeader><CardContent className="space-y-2 text-sm">
                <p>{t(`${prefix}historyCounts`, { fragments: value.stored_history_fragments, terms: value.indexed_terms, days: value.history_retention_days })}</p>
                {value.coverage.map((coverage, index) => <div key={index}>
                    <p>{t(`${prefix}coverage.${coverage.reason}`, { defaultValue: t(`${prefix}coverageOther`) })}{': '}{coverage.source_count}</p>
                    {coverage.indexed_through_unix_ms != null && <p>{t(`${prefix}indexedThrough`, { time: new Date(coverage.indexed_through_unix_ms).toLocaleString() })}</p>}
                </div>)}
            </CardContent></Card>


        </>}
    </AiAssistantHubLayout>;
}
