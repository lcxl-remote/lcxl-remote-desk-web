import { ModelThinkingSettings } from '@/components/model-thinking-settings';
import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2 } from 'lucide-react';
import { getApprovalModelProvider, updateApprovalModelProvider, testApprovalModelProvider, getModelProvider, reuseAiGatewayForApproval } from '@/services/clients';
import type { ApprovalModelPublic, ApprovalModelProbeParams, ApprovalModelUpdate, ApprovalModelReuseParams, ModelProviderPublic } from '@/services/types';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle } from '@/components/ui/alert-dialog';
import { useToast } from '@/hooks/use-toast';
import { APPROVAL_MODEL_ERROR_KEYS, deskErrorKey, errorCodeOf } from '@/lib/desk-error-i18n';

const LIMIT_FIELDS = [
    { name: 'runtime_max_output_tokens', labelKey: 'pages.aiModel.settings.runtimeBudget', minimum: 1, maximum: Number.MAX_SAFE_INTEGER },
    { name: 'max_context_bytes', labelKey: 'pages.aiModel.settings.maxContextBytes', minimum: 4096, maximum: 16777216 },
] as const;
type Editor = Omit<ApprovalModelProbeParams, 'api_key' | 'request_options'> & {
    enabled: boolean;
    api_key: string;
    clear_api_key: boolean;
    request_options_text: string;
};

function editorFrom(config: ApprovalModelPublic): Editor {
    return {
        enabled: config.enabled,
        wire_protocol: config.wire_protocol ?? 'open_ai_chat_completions',
        model: config.model ?? '',
        base_url: config.base_url ?? '',
        api_key: '',
        clear_api_key: false,
        request_options_text: JSON.stringify(config.request_options, null, 2),
        reasoning_contract: config.reasoning_contract ?? "conservative",
        anthropic_prefix_binding: config.anthropic_prefix_binding ?? false,
        output_limit_field: config.output_limit_field,
        runtime_max_output_tokens: config.runtime_max_output_tokens,
        max_context_bytes: config.max_context_bytes ?? 131072,
    };
}

function probeParams(editor: Editor): ApprovalModelProbeParams {
    const request_options: unknown = JSON.parse(editor.request_options_text);
    if (!request_options || typeof request_options !== 'object' || Array.isArray(request_options)) throw new Error();
    if (!editor.model.trim() || !editor.base_url.trim()
        || !['open_ai_chat_completions', 'anthropic_messages'].includes(editor.wire_protocol)
        || !['max_tokens', 'max_completion_tokens'].includes(editor.output_limit_field)
        || (editor.wire_protocol === 'anthropic_messages' && editor.output_limit_field !== 'max_tokens')
        || LIMIT_FIELDS.some(({ name, minimum, maximum }) => !Number.isSafeInteger(editor[name])
            || editor[name] < minimum || editor[name] > maximum)) {
        throw new Error();
    }
    return {
        wire_protocol: editor.wire_protocol,
        model: editor.model,
        base_url: editor.base_url,
        request_options,
        reasoning_contract: editor.reasoning_contract,
        anthropic_prefix_binding: editor.anthropic_prefix_binding,
        output_limit_field: editor.output_limit_field,
        runtime_max_output_tokens: editor.runtime_max_output_tokens,
        max_context_bytes: editor.max_context_bytes,
        ...(editor.clear_api_key ? { api_key: '' } : editor.api_key.trim() ? { api_key: editor.api_key } : {}),
    };
}

function canReuseGateway(config: ModelProviderPublic | null): boolean {
    return !!config && !!config.model?.trim() && !!config.base_url?.trim() && config.api_key_set
        && ['open_ai_chat_completions', 'anthropic_messages'].includes(config.wire_protocol ?? '')
        && LIMIT_FIELDS.every(({ name, minimum, maximum }) => {
            const value = config[name];
            return typeof value === 'number' && Number.isSafeInteger(value) && value >= minimum && value <= maximum;
        });
}

function canProbeSavedApproval(config: ApprovalModelPublic | null): boolean {
    if (!config?.api_key_set || !config.wire_protocol || config.max_context_bytes == null) return false;
    try { probeParams(editorFrom(config)); return true; }
    catch { return false; }
}

export function ApprovalModelSettings() {
    const { t } = useTranslation();
    const { toast } = useToast();
    const [config, setConfig] = useState<ApprovalModelPublic | null>(null);
    const [editor, setEditor] = useState<Editor | null>(null);
    const [loading, setLoading] = useState(true);
    const [busy, setBusy] = useState(false);
    const [gateway, setGateway] = useState<ModelProviderPublic | null>(null);
    const [gatewayLoading, setGatewayLoading] = useState(true);
    const [gatewayError, setGatewayError] = useState(false);
    const [reuseConfirmation, setReuseConfirmation] = useState<ApprovalModelReuseParams | null>(null);
    const apply = (fresh: ApprovalModelPublic) => { setConfig(fresh); setEditor(editorFrom(fresh)); };
    const succeed = (description: string) => toast({ title: t('pages.system.settings.success'), description });
    const failure = (caught: unknown, fallback: string) => {
        const key = deskErrorKey(APPROVAL_MODEL_ERROR_KEYS, errorCodeOf(caught));
        const detail = caught instanceof Error ? caught.message.replace(/^Custom desk error\(-?\d+\):\s*/, '') : '';
        toast({ variant: 'destructive', title: fallback,
            description: key ? t(key, { tokens: config?.runtime_max_output_tokens }) : detail || fallback,
            duration: 15000 });
    };

    useEffect(() => {
        let cancelled = false;
        void getApprovalModelProvider().then(result => {
            if (cancelled) return;
            if (!result.success || !result.data) { failure(null, t('pages.approvalModel.loadFailed')); return; }
            apply(result.data);
        }).catch(caught => { if (!cancelled) failure(caught, t('pages.approvalModel.loadFailed')); })
            .finally(() => { if (!cancelled) setLoading(false); });
        void getModelProvider().then(result => {
            if (cancelled) return;
            setGateway(result.success && result.data ? result.data : null);
            setGatewayError(!result.success || !result.data);
        }).catch(() => { if (!cancelled) { setGateway(null); setGatewayError(true); } })
            .finally(() => { if (!cancelled) setGatewayLoading(false); });
        return () => { cancelled = true; };
    }, [t]);

    const reload = async () => {
        setBusy(true); setGatewayLoading(true);
        try {
            const [approval, source] = await Promise.allSettled([getApprovalModelProvider(), getModelProvider()]);
            if (source.status === 'fulfilled' && source.value.success && source.value.data) {
                setGateway(source.value.data); setGatewayError(false);
            } else { setGateway(null); setGatewayError(true); }
            if (approval.status === 'rejected') throw approval.reason;
            const result = approval.value;
            if (!result.success || !result.data) throw new Error(result.message || t('pages.approvalModel.loadFailed'));
            apply(result.data);
        } catch (caught) { failure(caught, t('pages.approvalModel.loadFailed')); }
        finally { setBusy(false); setGatewayLoading(false); }
    };
    const dirty = !!config && !!editor && JSON.stringify(editor) !== JSON.stringify(editorFrom(config));
    const canTest = !busy && !dirty && canProbeSavedApproval(config);
    const change = <K extends keyof Editor>(key: K, value: Editor[K]) => {
        setEditor(current => current && { ...current, [key]: value });
    };
    const save = async (event: FormEvent) => {
        event.preventDefault();
        if (!config || !editor || busy) return;
        let params: ApprovalModelProbeParams;
        try { params = probeParams(editor); }
        catch { failure(null, t('pages.approvalModel.invalid')); return; }
        setBusy(true);
        try {
            const payload: ApprovalModelUpdate = {
                ...params, enabled: editor.enabled,
                expected_configuration_revision: config.configuration_revision,
                expected_connection_revision: config.connection_revision,
                expected_profile_revision: config.profile_revision,
            };
            const result = await updateApprovalModelProvider(payload);
            if (!result.success || !result.data) throw new Error(result.message || t('pages.approvalModel.saveFailed'));
            apply(result.data); succeed(t(result.data.available ? 'pages.approvalModel.savedReady' : 'pages.approvalModel.saved'));
        } catch (caught) { failure(caught, t('pages.approvalModel.saveFailed')); }
        finally { setBusy(false); }
    };
    const test = async () => {
        if (!editor || !canTest) return;
        setBusy(true);
        try {
            const result = await testApprovalModelProvider(probeParams(editor));
            if (!result.success || !result.data) throw new Error(result.message || t('pages.approvalModel.testFailed'));
            if (!result.data.saved_as_current) throw new Error(t('pages.approvalModel.testStale'));
            const fresh = await getApprovalModelProvider();
            if (!fresh.success || !fresh.data) throw new Error(t('pages.approvalModel.loadFailed'));
            apply(fresh.data);
            succeed(t('pages.approvalModel.testPassed', { count: result.data.validated_capabilities.length, latency: result.data.latency_ms }));
        } catch (caught) { failure(caught, t('pages.approvalModel.testFailed')); }
        finally { setBusy(false); }
    };
    const reuse = async (params: ApprovalModelReuseParams) => {
        if (busy) return;
        setBusy(true);
        try {
            const result = await reuseAiGatewayForApproval(params);
            if (!result.success || !result.data) throw new Error(result.message || t('pages.approvalModel.reuseFailed'));
            apply(result.data); succeed(t('pages.approvalModel.reused'));
        } catch (caught) { failure(caught, t('pages.approvalModel.reuseFailed')); }
        finally { setBusy(false); }
    };
    const requestReuse = () => {
        if (!config || busy || gatewayLoading || !canReuseGateway(gateway)) return;
        const params: ApprovalModelReuseParams = {
            expected_configuration_revision: config.configuration_revision,
            expected_connection_revision: config.connection_revision,
            expected_profile_revision: config.profile_revision,
        };
        if (config.wire_protocol || config.model?.trim() || config.base_url?.trim() || config.api_key_set) {
            setReuseConfirmation(params);
        } else { void reuse(params); }
    };

    if (loading) return <div className="flex justify-center p-8"><Loader2 className="h-8 w-8 animate-spin" aria-label={t('common.loading')} /></div>;
    return <div className="container mx-auto max-w-4xl space-y-6 px-4 py-8">
        <div>
            <h1 className="text-3xl font-bold">{t('pages.approvalModel.title')}</h1>
            <p className="mt-2 text-muted-foreground">{t('pages.approvalModel.description')}</p>
            <p className="mt-2 text-sm">{t('pages.approvalModel.steps')}</p>
        </div>
        <Button type="button" variant="outline" onClick={() => void reload()} disabled={busy}>{t('pages.approvalModel.reload')}</Button>
        {config && editor && <Card>
            <CardHeader>
                <CardTitle>{t('pages.aiModel.settings.gateway')}</CardTitle>
                <CardDescription>{config.available ? t('pages.approvalModel.available')
                    : t(`pages.aiAssistant.approvalModelReason.${config.unavailable_reason ?? 'unknown'}`)}</CardDescription>
            </CardHeader>
            <CardContent>
                <form onSubmit={event => void save(event)} className="space-y-6">
                    <fieldset disabled={busy} className="space-y-6">
                        <div className="flex items-center justify-between gap-4">
                            <Label htmlFor="approval-enabled">{t('pages.approvalModel.enabled')}</Label>
                            <Switch id="approval-enabled" checked={editor.enabled} onCheckedChange={value => change('enabled', value)} disabled={busy} />
                        </div>
                        <div className="space-y-2">
                            <Label htmlFor="approval-protocol">{t('pages.aiModel.settings.provider')}</Label>
                            <Select value={editor.wire_protocol} onValueChange={value => {
                                change('wire_protocol', value);
                                change('reasoning_contract', 'conservative');
                                change('anthropic_prefix_binding', false);
                                if (value === 'anthropic_messages') change('output_limit_field', 'max_tokens');
                            }} disabled={busy}>
                                <SelectTrigger id="approval-protocol"><SelectValue /></SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="open_ai_chat_completions">{t('pages.aiModel.settings.provider.openaiCompatible')}</SelectItem>
                                    <SelectItem value="anthropic_messages">{t('pages.aiModel.settings.provider.anthropic')}</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div className="space-y-2">
                            <Label htmlFor="approval-model">{t('pages.aiModel.settings.model')}</Label>
                            <Input id="approval-model" value={editor.model} onChange={event => change('model', event.target.value)} required />
                        </div>
                        <div className="space-y-2">
                            <Label htmlFor="approval-url">{t('pages.aiModel.settings.baseUrl')}</Label>
                            <Input id="approval-url" value={editor.base_url} onChange={event => change('base_url', event.target.value)} required />
                            <p className="text-sm text-muted-foreground">{t(editor.wire_protocol === 'anthropic_messages' ? 'pages.aiModel.settings.baseUrl.anthropic' : 'pages.aiModel.settings.baseUrl.openai')}</p>
                        </div>
                        <div className="space-y-2">
                            <Label htmlFor="approval-key">{t('pages.aiModel.settings.apiKey')}</Label>
                            <Input id="approval-key" type="password" autoComplete="off" value={editor.api_key}
                                placeholder={t(config.api_key_set ? 'pages.aiModel.settings.apiKeySet' : 'pages.aiModel.settings.apiKeyUnset')}
                                disabled={editor.clear_api_key || busy} onChange={event => change('api_key', event.target.value)} />
                            <p className="text-sm text-muted-foreground">{t('pages.approvalModel.keyDescription')}</p>
                        </div>
                        {config.api_key_set && <div className="flex items-center justify-between gap-4">
                            <Label htmlFor="approval-clear-key">{t('pages.aiModel.settings.clearApiKey')}</Label>
                            <Switch id="approval-clear-key" checked={editor.clear_api_key} onCheckedChange={value => change('clear_api_key', value)} disabled={busy} />
                        </div>}
                        <div className="space-y-2">
                            <Label htmlFor="approval-options">{t('pages.aiModel.settings.requestOptions')}</Label>
                            <Textarea id="approval-options" className="min-h-32 font-mono" value={editor.request_options_text} onChange={event => change('request_options_text', event.target.value)} />
                        </div>
                        <div className="space-y-2">
                            <Label htmlFor="approval-output-field">{t('pages.aiModel.settings.outputLimitField')}</Label>
                            <Select value={editor.output_limit_field} onValueChange={value => change('output_limit_field', value)} disabled={busy}>
                                <SelectTrigger id="approval-output-field"><SelectValue /></SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="max_tokens">max_tokens</SelectItem>
                                    <SelectItem value="max_completion_tokens" disabled={editor.wire_protocol === 'anthropic_messages'}>max_completion_tokens</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div className="grid gap-4 md:grid-cols-2">
                            <ModelThinkingSettings protocol={editor.wire_protocol} contract={editor.reasoning_contract} prefixBinding={editor.anthropic_prefix_binding}
                            value={editor.request_options_text} onContractChange={value => change('reasoning_contract', value)}
                            onPrefixBindingChange={value => change('anthropic_prefix_binding', value)} onChange={value => change('request_options_text', value)} />
                        {LIMIT_FIELDS.map(({ name, labelKey, minimum, maximum }) => <div key={name} className="space-y-2">
                                <Label htmlFor={`approval-${name}`}>{t(labelKey)}</Label>
                                <Input id={`approval-${name}`} type="number" min={minimum} max={maximum} step={1} required value={editor[name]}
                                    onChange={event => change(name, Number(event.target.value))} />
                            </div>)}
                        </div>
                        <p className="text-sm text-muted-foreground">{t('pages.aiModel.settings.runtimeBudgetHint')}</p>
                    </fieldset>
                    <p className="text-sm text-muted-foreground">{t(gatewayLoading ? 'common.loading'
                        : gatewayError ? 'pages.approvalModel.reuseLoadFailed'
                            : canReuseGateway(gateway) ? 'pages.approvalModel.reuseDescription' : 'pages.approvalModel.reuseUnavailable')}</p>
                    <p id="approval-test-description" className="text-sm text-muted-foreground">{t(dirty ? 'pages.approvalModel.testSaveRequired'
                        : !canProbeSavedApproval(config) ? 'pages.approvalModel.testNeedsConfig' : 'pages.approvalModel.testDescription')}</p>
                    <div className="flex flex-wrap gap-3">
                        <Button type="submit" disabled={busy || !dirty}>{t('common.save')}</Button>
                        <Button type="button" variant="outline" onClick={requestReuse} disabled={busy || gatewayLoading || !canReuseGateway(gateway)}>
                            {t('pages.approvalModel.reuse')}
                        </Button>
                        <Button type="button" variant="outline" disabled={!canTest} aria-describedby="approval-test-description"
                            onClick={() => void test()}>{t('pages.approvalModel.test')}</Button>
                        {busy && <Loader2 className="h-5 w-5 animate-spin" aria-label={t('common.loading')} />}
                    </div>
                </form>
            </CardContent>
        </Card>}
        <AlertDialog open={reuseConfirmation !== null} onOpenChange={open => { if (!open) setReuseConfirmation(null); }}>
            <AlertDialogContent>
                <AlertDialogHeader>
                    <AlertDialogTitle>{t('pages.approvalModel.reuseConfirmTitle')}</AlertDialogTitle>
                    <AlertDialogDescription>{t('pages.approvalModel.reuseConfirmDescription')}</AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                    <AlertDialogCancel>{t('common.cancel')}</AlertDialogCancel>
                    <AlertDialogAction disabled={busy} onClick={() => { if (reuseConfirmation) void reuse(reuseConfirmation); }}>
                        {t('pages.approvalModel.reuseConfirm')}
                    </AlertDialogAction>
                </AlertDialogFooter>
            </AlertDialogContent>
        </AlertDialog>
    </div>;
}
