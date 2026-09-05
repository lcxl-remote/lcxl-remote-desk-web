import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import type { SearchConfigPublic, SearchConfigUpdate, SearchProvider, SearchTestResult } from '@/services/types';

export interface WebSearchSettingsApi {
    load(): Promise<SearchConfigPublic>;
    save(update: SearchConfigUpdate): Promise<SearchConfigPublic>;
    test(update: SearchConfigUpdate): Promise<SearchTestResult>;
}

export function WebSearchForm({ api }: { api: WebSearchSettingsApi }) {
    const { t } = useTranslation();
    const [config, setConfig] = useState<SearchConfigPublic | null>(null);
    const [provider, setProvider] = useState<SearchProvider>('duck_duck_go');
    const [keyMode, setKeyMode] = useState<'keep' | 'replace' | 'clear'>('keep');
    const [key, setKey] = useState('');
    const [busy, setBusy] = useState(false);
    const [status, setStatus] = useState('');
    const [testResult, setTestResult] = useState<SearchTestResult | null>(null);
    const epoch = useRef(0);
    const pending = useRef(false);
    const apply = (value: SearchConfigPublic) => {
        setConfig(value); setProvider(value.provider); setKey(''); setKeyMode('keep');
    };
    const load = useCallback(async () => {
        const version = ++epoch.current;
        pending.current = true; setBusy(true); setStatus(''); setTestResult(null);
        setConfig(null); setKey('');
        try {
            const value = await api.load();
            if (version === epoch.current) apply(value);
        } catch {
            if (version === epoch.current) setStatus('loadFailed');
        } finally {
            if (version === epoch.current) { pending.current = false; setBusy(false); }
        }
    }, [api]);
    useEffect(() => { void load(); return () => { ++epoch.current; pending.current = false; }; }, [load]);
    const needsKey = config?.providers.find((item) => item.provider === provider)?.requires_api_key ?? false;
    const payload = (): SearchConfigUpdate => ({
        expected_revision: config!.revision,
        provider,
        api_key: !needsKey || keyMode === 'clear' ? '' : keyMode === 'replace' ? key : null,
    });
    const execute = async (operation: 'save' | 'test') => {
        if (!config || pending.current) return;
        pending.current = true; setBusy(true); setStatus(''); setTestResult(null);
        const version = epoch.current;
        try {
            if (operation === 'save') {
                const value = await api.save(payload());
                if (version === epoch.current) { apply(value); setStatus('saved'); }
            } else {
                const result = await api.test(payload());
                if (version === epoch.current) setTestResult(result);
            }
        } catch {
            if (version === epoch.current) {
                setStatus(operation === 'save' ? 'saveFailed' : 'testFailed');
                // An uncertain save must be re-read before another mutation.
                if (operation === 'save') { setConfig(null); setKey(''); }
            }
        } finally {
            if (version === epoch.current) { pending.current = false; setBusy(false); }
        }
    };
    const invalidKey = needsKey && keyMode === 'replace' && !key.trim();
    return <div className="p-4 max-w-3xl space-y-4">
        <Card>
            <CardHeader>
                <CardTitle>{t('pages.webSearch.title')}</CardTitle>
                <CardDescription>{t('pages.webSearch.description')}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-4">
                <Button variant="outline" disabled={busy} onClick={() => void load()}>{t('pages.webSearch.reload')}</Button>
                {config && <>
                    <label className="block space-y-2">
                        <span>{t('pages.webSearch.provider')}</span>
                        <select className="block w-full rounded border bg-background p-2" value={provider} disabled={busy} onChange={(event) => {
                            setProvider(event.target.value as SearchProvider); setKey(''); setKeyMode('replace'); setStatus(''); setTestResult(null);
                        }}>
                            {config.providers.map((item) => <option key={item.provider} value={item.provider}>{item.display_name}</option>)}
                        </select>
                    </label>
                    {!needsKey ? <p>{t('pages.webSearch.noKey')}</p> : <>
                        <label className="block space-y-2">
                            <span>{t('pages.webSearch.keyAction')}</span>
                            <select className="block w-full rounded border bg-background p-2" value={keyMode} disabled={busy} onChange={(event) => { setKeyMode(event.target.value as typeof keyMode); setKey(''); }}>
                                <option value="keep" disabled={provider !== config.provider}>{t('pages.webSearch.keepKey')}</option>
                                <option value="replace">{t('pages.webSearch.replaceKey')}</option>
                                <option value="clear">{t('pages.webSearch.clearKey')}</option>
                            </select>
                        </label>
                        {keyMode === 'replace' && <label className="block space-y-2">
                            <span>{t('pages.webSearch.apiKey')}</span>
                            <Input type="password" autoComplete="new-password" maxLength={4096} value={key} disabled={busy} onChange={(event) => setKey(event.target.value)} />
                        </label>}
                        <p className="text-sm text-muted-foreground">{t('pages.webSearch.keyPrivate')}</p>
                    </>}
                    <p className="text-sm">{t(config.configured ? 'pages.webSearch.configured' : 'pages.webSearch.unconfigured')}</p>
                    <p className="text-sm text-muted-foreground">{t('pages.webSearch.testNotice')}</p>
                    <div className="flex gap-2">
                        <Button disabled={busy || invalidKey} onClick={() => void execute('save')}>{t('pages.webSearch.save')}</Button>
                        <Button variant="outline" disabled={busy || invalidKey} onClick={() => void execute('test')}>{t('pages.webSearch.test')}</Button>
                    </div>
                </>}
                <div role="status" aria-live="polite">
                    {busy && <p>{t('pages.webSearch.busy')}</p>}
                    {status && <p>{t(`pages.webSearch.${status}`)}</p>}
                    {testResult && <p>{t('pages.webSearch.testPassed', { count: testResult.result_count, milliseconds: testResult.latency_ms })}</p>}
                </div>
            </CardContent>
        </Card>
    </div>;
}
