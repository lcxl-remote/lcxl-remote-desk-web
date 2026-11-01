import { assistantSourceHref } from '@/components/markdown-source-links';
import { Link } from 'react-router-dom';
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RefreshCw, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { SelectItem } from '@/components/ui/select';
import { SelectField } from '@/components/ui/select-field';
import { AlertDialog, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
    AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog';
import type { AssistantSourceResolution, MemoryListResult, MemoryRecord } from '@/services/types';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';
import { assistantDataControlId } from './assistant-native-ids';
import { assistantDataErrorKey, clearAssistantMemories, deleteAssistantMemory,
    listAssistantMemories, readAssistantSource } from './assistant-data-api';

type Intent = { kind: 'delete'; operationId: string; memory: MemoryRecord }
    | { kind: 'clear'; operationId: string; generation: number };

function date(value: number | null | undefined, locale: string, timezone?: string | null): string {
    if (value == null || !Number.isFinite(value)) return '';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return '';
    try { return parsed.toLocaleString(locale, timezone ? { timeZone: timezone } : undefined); }
    catch { return ''; }
}

export default function AiAssistantMemories() {
    const { t, i18n } = useTranslation();
    const [page, setPage] = useState<MemoryListResult | null>(null);
    const [scope, setScope] = useState<'all' | 'personal' | 'device'>('all');
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState('');
    const [notice, setNotice] = useState('');
    const [intent, setIntent] = useState<Intent | null>(null);
    const [saving, setSaving] = useState(false);
    const [sourceId, setSourceId] = useState<string | null>(null);
    const [source, setSource] = useState<AssistantSourceResolution | null>(null);
    const [sourceError, setSourceError] = useState('');
    const epoch = useRef(0);
    const mutation = useRef(false);
    const load = useCallback(async (previous?: MemoryListResult) => {
        const own = ++epoch.current;
        setLoading(true); setError('');
        try {
            const result = await listAssistantMemories({ scope: scope === 'all' ? undefined : scope,
                ...(previous?.next_cursor ? { cursor: previous.next_cursor,
                    generation: previous.memory_state_generation, controlGeneration: previous.control_generation } : {}) });
            if (epoch.current !== own) return;
            setPage(previous ? { ...result, memories: [...previous.memories,
                ...result.memories.filter(value => !previous.memories.some(old => old.memory_id === value.memory_id))] } : result);
        } catch (cause) {
            if (epoch.current !== own) return;
            setPage(null); setError(assistantDataErrorKey(cause));
        } finally { if (epoch.current === own) setLoading(false); }
    }, [scope]);

    useEffect(() => {
        setPage(null); setNotice(''); setIntent(null); setSourceId(null); setSource(null);
        void load();
        return () => { epoch.current += 1; };
    }, [load]);

    useEffect(() => {
        if (!sourceId) { setSource(null); setSourceError(''); return; }
        let active = true;
        setSource(null); setSourceError('');
        readAssistantSource(sourceId).then(value => { if (active) setSource(value); })
            .catch(cause => { if (active) setSourceError(assistantDataErrorKey(cause)); });
        return () => { active = false; };
    }, [sourceId]);

    const confirm = async () => {
        if (!intent || mutation.current) return;
        mutation.current = true; setSaving(true); setError(''); setNotice('');
        setSourceId(null); setSource(null);
        try {
            const receipt = intent.kind === 'delete'
                ? await deleteAssistantMemory({ memory_id: intent.memory.memory_id,
                    expected_revision: intent.memory.revision, operation_id: intent.operationId })
                : await clearAssistantMemories({ operation_id: intent.operationId, expected_memory_state_generation: intent.generation });
            setIntent(null);
            setNotice(receipt.outcome === 'stale_target' ? 'pages.aiAssistant.memory.changed'
                : intent.kind === 'delete' ? 'pages.aiAssistant.memory.deleted' : 'pages.aiAssistant.memory.cleared');
            await load();
        } catch (cause) { setError(assistantDataErrorKey(cause)); }
        finally { mutation.current = false; setSaving(false); }
    };

    return <AiAssistantHubLayout>
        <div className="flex flex-wrap items-start justify-between gap-3">
            <h1 className="text-2xl font-semibold">{t('pages.aiAssistant.memory.title')}</h1>
            <div className="flex flex-wrap gap-2">
                <Button variant="outline" disabled={loading || saving} onClick={() => { setIntent(null); setSourceId(null); void load(); }}>
                    <RefreshCw className="mr-2 size-4" aria-hidden="true" />{t('pages.aiAssistant.memory.refresh')}
                </Button>
                <Button variant="destructive" disabled={!page || loading || saving}
                    onClick={() => page && setIntent({ kind: 'clear', operationId: assistantDataControlId(), generation: page.memory_state_generation })}>
                    {t('pages.aiAssistant.memory.clear')}
                </Button>
            </div>
        </div>
        <label className="flex flex-wrap items-center gap-3 text-sm">
            {t('pages.aiAssistant.memory.scope')}
            <SelectField aria-label={t('pages.aiAssistant.memory.scope')} className="h-9 w-40" value={scope} disabled={saving}
                onValueChange={value => setScope(value as typeof scope)}>
                <SelectItem value="all">{t('pages.aiAssistant.memory.scopeAll')}</SelectItem>
                <SelectItem value="personal">{t('pages.aiAssistant.memory.scopePersonal')}</SelectItem>
                <SelectItem value="device">{t('pages.aiAssistant.memory.scopeDevice')}</SelectItem>
            </SelectField>
        </label>
        {error && !intent && <p role="alert" className="text-sm text-destructive">{t(error)}</p>}
        {notice && <p role="status" className="text-sm">{t(notice)}</p>}
        {loading && <p role="status" className="text-sm text-muted-foreground">{t('pages.aiAssistant.memory.loading')}</p>}
        {!loading && page?.memories.length === 0 && <p>{t('pages.aiAssistant.memory.empty')}</p>}
        <div className="space-y-3">
            {page?.memories.map(memory => <Card key={`${memory.memory_id}:${memory.revision}`}>
                <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 space-y-0 pb-3">
                    <div className="min-w-0 flex-1 space-y-2">
                        <CardTitle className="whitespace-pre-wrap break-words text-base font-medium">{memory.content}</CardTitle>
                        {memory.context && <p className="whitespace-pre-wrap break-words text-sm text-muted-foreground">{memory.context}</p>}
                    </div>
                    <Button variant="ghost" size="sm" disabled={loading || saving}
                        onClick={() => setIntent({ kind: 'delete', operationId: assistantDataControlId(), memory })}>
                        <Trash2 className="mr-2 size-4" aria-hidden="true" />{t('pages.aiAssistant.memory.delete')}
                    </Button>
                </CardHeader>
                <CardContent className="space-y-3">
                    <dl className="flex flex-wrap gap-x-6 gap-y-2 text-xs text-muted-foreground">
                        <div><dt className="inline">{t('pages.aiAssistant.memory.scope')}{': '}</dt><dd className="inline">{memory.scope === 'personal'
                            ? t('pages.aiAssistant.memory.scopePersonal') : memory.source_device?.current_name
                                ?? memory.source_device?.recorded_name ?? t('pages.aiAssistant.memory.scopeDevice')}</dd></div>
                        <div><dt className="inline">{t('pages.aiAssistant.memory.version')}{': '}</dt><dd className="inline">{memory.revision}</dd></div>
                        <div><dt className="inline">{t('pages.aiAssistant.memory.updated')}{': '}</dt><dd className="inline">{date(memory.updated_at_unix_ms, i18n.language)}</dd></div>
                        <div><dt className="inline">{t('pages.aiAssistant.memory.observed')}{': '}</dt><dd className="inline">{date(memory.observed_at_unix_ms, i18n.language)}</dd></div>
                    </dl>
                    {memory.status !== 'active' && <p className="text-sm text-muted-foreground">{t(`pages.aiAssistant.memory.status.${memory.status}`)}</p>}
                    <div className="flex flex-wrap gap-2">
                        {memory.source_ids.map((id, index) => <Button key={id} size="sm" variant="outline" disabled={saving}
                            onClick={() => setSourceId(id)}>{t('pages.aiAssistant.memory.source', { index: index + 1 })}</Button>)}
                    </div>
                </CardContent>
            </Card>)}
        </div>
        {page?.next_cursor && <Button variant="outline" disabled={loading || saving} onClick={() => void load(page)}>
            {t('pages.aiAssistant.memory.more')}
        </Button>}
        <AlertDialog open={intent !== null} onOpenChange={open => { if (!open && !saving) setIntent(null); }}>
            <AlertDialogContent>
                <AlertDialogHeader>
                    <AlertDialogTitle>{t(intent?.kind === 'clear' ? 'pages.aiAssistant.memory.clearConfirm' : 'pages.aiAssistant.memory.deleteConfirm')}</AlertDialogTitle>
                    <AlertDialogDescription>{t('pages.aiAssistant.memory.historyKept')}</AlertDialogDescription>
                </AlertDialogHeader>
                {intent?.kind === 'delete' && <p className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words text-sm">{intent.memory.content}</p>}
                {intent?.kind === 'clear' && <p className="text-sm">{t('pages.aiAssistant.memory.clearExplanation')}</p>}
                {error && <p role="alert" className="text-sm text-destructive">{t(error)}</p>}
                <AlertDialogFooter>
                    <AlertDialogCancel disabled={saving}>{t('pages.aiAssistant.memory.cancel')}</AlertDialogCancel>
                    <Button variant="destructive" disabled={saving} onClick={() => void confirm()}>
                        {t(saving ? 'pages.aiAssistant.memory.deleting' : intent?.kind === 'clear' ? 'pages.aiAssistant.memory.clear' : 'pages.aiAssistant.memory.delete')}
                    </Button>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
        <AlertDialog open={sourceId !== null} onOpenChange={open => { if (!open) { setSourceId(null); setSource(null); } }}>
            <AlertDialogContent aria-describedby={source?.availability === 'evidence_only' ? 'assistant-memory-evidence' : undefined}>
                <AlertDialogHeader>
                    <AlertDialogTitle>{t('pages.aiAssistant.memory.sourceTitle')}</AlertDialogTitle>
                    {source?.availability === 'evidence_only' && <AlertDialogDescription id="assistant-memory-evidence">
                        {t('pages.aiAssistant.memory.evidenceOnly')}
                    </AlertDialogDescription>}
                </AlertDialogHeader>
                {sourceError && <p role="alert" className="text-sm text-destructive">{t(sourceError)}</p>}
                {!source && !sourceError && <p role="status">{t('pages.aiAssistant.memory.loading')}</p>}
                {source?.availability === 'unavailable' && <p>{t('pages.aiAssistant.memory.sourceUnavailable')}</p>}
                {source?.recorded_at_unix_ms != null && <p className="text-sm text-muted-foreground">
                    {t('pages.aiAssistant.memory.observed')} {date(source.recorded_at_unix_ms, i18n.language, source.statement_timezone)}
                </p>}
                {source?.statement_timezone && <p className="text-sm text-muted-foreground">
                    {t('pages.aiAssistant.memory.statementTimezone', { timezone: source.statement_timezone })}
                </p>}
                {source?.content && <pre className="max-h-[50vh] overflow-y-auto whitespace-pre-wrap break-words font-sans text-sm">{source.content}</pre>}
                {source?.availability === 'partial' && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.memory.sourcePartial')}</p>}
                {source?.availability === 'redacted' && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.memory.sourceRedacted')}</p>}
                <AlertDialogFooter>
                    {source?.locator && ['available', 'partial', 'redacted'].includes(source.availability) && <Button asChild variant="outline">
                        <Link to={assistantSourceHref(source.source_id, source.revision)} target="_blank" rel="noopener noreferrer">{t('pages.aiAssistant.memory.openOriginal')}</Link>
                    </Button>}
                    <AlertDialogCancel>{t('pages.aiAssistant.memory.close')}</AlertDialogCancel>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
    </AiAssistantHubLayout>;
}
