import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';

export type ReasoningContract = 'conservative' | 'openai_chat' | 'deepseek_chat' | 'anthropic_messages';

export function updateThinkingOption(text: string, key: string, value: unknown): string {
    const options = JSON.parse(text);
    if (!options || typeof options !== 'object' || Array.isArray(options)) throw new Error('Invalid request options');
    if (value === undefined) delete options[key];
    else options[key] = value;
    return JSON.stringify(options, null, 2);
}

export function ModelThinkingSettings({ protocol, contract, prefixBinding, value, onContractChange, onPrefixBindingChange, onChange }: {
    protocol: string; contract: string; prefixBinding: boolean; value: string;
    onContractChange: (value: ReasoningContract) => void;
    onPrefixBindingChange: (value: boolean) => void;
    onChange: (value: string) => void;
}) {
    const { t } = useTranslation();
    let options: Record<string, unknown> | null = null;
    try { const parsed = JSON.parse(value); if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) options = parsed; } catch { /* JSON editor reports invalid input. */ }
    const anthropic = protocol === 'anthropic_messages';
    const contracts: ReasoningContract[] = anthropic ? ['conservative', 'anthropic_messages'] : ['conservative', 'openai_chat', 'deepseek_chat'];
    const thinking = options?.thinking as { type?: string; budget_tokens?: number; display?: string } | undefined;
    const output = options?.output_config as { effort?: string } | undefined;
    const context = options?.context_management as { edits?: { keep?: 'all' | { type: string; value: number } }[] } | undefined;
    const keep = context?.edits?.[0]?.keep;
    const efforts = anthropic ? ['low', 'medium', 'high', 'max']
        : contract === 'deepseek_chat' ? ['low', 'high', 'max'] : ['none', 'minimal', 'low', 'medium', 'high', 'xhigh'];
    const update = (key: string, option: unknown) => onChange(updateThinkingOption(value, key, option));
    const clearing = (keep: 'all' | number) => update('context_management', { edits: [{ type: 'clear_thinking_20251015', keep: keep === 'all' ? 'all' : { type: 'thinking_turns', value: keep } }] });
    const effortKey = anthropic ? 'output_config' : 'reasoning_effort';
    return <section className="space-y-3 rounded-md border p-3">
        <label className="space-y-1 block text-sm font-medium">
            <span>{t('pages.aiModel.thinking.contract')}</span>
            <Select value={contract} onValueChange={(next: ReasoningContract) => { onContractChange(next); if (next !== 'anthropic_messages') onPrefixBindingChange(false); }}>
                <SelectTrigger aria-label={t('pages.aiModel.thinking.contract')}><SelectValue /></SelectTrigger>
                <SelectContent>{contracts.map(contract => <SelectItem key={contract} value={contract}>{t(`pages.aiModel.thinking.contract.${contract}`)}</SelectItem>)}</SelectContent>
            </Select>
        </label>
        <p className="text-xs text-muted-foreground">{t('pages.aiModel.thinking.contractHint')}</p>
        {!options ? <p className="text-sm text-destructive">{t('pages.aiModel.thinking.invalidJson')}</p> : <>
            {(anthropic || contract !== 'openai_chat') && <label className="space-y-1 block text-sm">
                <span>{t('pages.aiModel.thinking.mode')}</span>
                <Select value={thinking?.type ?? 'default'} onValueChange={mode => {
                    let next = updateThinkingOption(value, 'thinking', mode === 'default' ? undefined
                        : mode === 'enabled' && anthropic ? { type: mode, budget_tokens: thinking?.budget_tokens ?? 2048 }
                            : { type: mode });
                    if (anthropic && mode !== 'adaptive') next = updateThinkingOption(next, 'output_config', undefined);
                    onChange(next);
                }}>
                    <SelectTrigger aria-label={t('pages.aiModel.thinking.mode')}><SelectValue /></SelectTrigger>
                    <SelectContent>{['default', 'disabled', 'enabled', ...(anthropic ? ['adaptive'] : [])].map(mode =>
                        <SelectItem key={mode} value={mode}>{t(`pages.aiModel.thinking.mode.${mode}`)}</SelectItem>)}</SelectContent>
                </Select>
            </label>}
            {anthropic && thinking?.type === 'enabled' && <label className="space-y-1 block text-sm">
                <span>{t('pages.aiModel.thinking.manualBudget')}</span>
                <Input type="number" min={1} value={thinking.budget_tokens ?? ''} aria-label={t('pages.aiModel.thinking.manualBudget')}
                    onChange={event => update('thinking', { ...thinking, budget_tokens: Number(event.target.value) })} />
            </label>}
            {(!anthropic || thinking?.type === 'adaptive') && <label className="space-y-1 block text-sm">
                <span>{t('pages.aiModel.thinking.effort')}</span>
                <Select value={(anthropic ? output?.effort : options.reasoning_effort as string) ?? 'default'} onValueChange={effort => update(effortKey,
                    effort === 'default' ? undefined : anthropic ? { ...output, effort } : effort)}>
                    <SelectTrigger aria-label={t('pages.aiModel.thinking.effort')}><SelectValue /></SelectTrigger>
                    <SelectContent><SelectItem value="default">{t('pages.aiModel.thinking.mode.default')}</SelectItem>
                        {efforts.map(effort => <SelectItem key={effort} value={effort}>{effort}</SelectItem>)}</SelectContent>
                </Select>
            </label>}
            <p className="text-xs text-muted-foreground">{t('pages.aiModel.thinking.effortHint')}</p>
            {anthropic && <>
                <label className="flex items-center gap-2 text-sm"><Switch checked={!!context} onCheckedChange={enabled => enabled ? clearing(1) : update('context_management', undefined)} />{t('pages.aiModel.thinking.clear')}</label>
                {!!context && <label className="space-y-1 block text-sm">
                    <span>{t('pages.aiModel.thinking.keep')}</span>
                    <Select value={keep === 'all' ? 'all' : 'turns'} onValueChange={mode => clearing(mode === 'all' ? 'all' : 1)}>
                        <SelectTrigger aria-label={t('pages.aiModel.thinking.keep')}><SelectValue /></SelectTrigger>
                        <SelectContent><SelectItem value="turns">{t('pages.aiModel.thinking.keepTurns')}</SelectItem><SelectItem value="all">{t('pages.aiModel.thinking.keepAll')}</SelectItem></SelectContent>
                    </Select>
                    {keep !== 'all' && <Input type="number" min={1} value={keep?.value ?? ''} aria-label={t('pages.aiModel.thinking.keepTurns')} onChange={event => clearing(Number(event.target.value))} />}
                </label>}
                <p className="text-xs text-muted-foreground">{t('pages.aiModel.thinking.clearHint')}</p>
                <label className="flex items-center gap-2 text-sm"><Switch checked={prefixBinding} disabled={contract !== 'anthropic_messages'} onCheckedChange={onPrefixBindingChange} />{t('pages.aiModel.thinking.prefix')}</label>
                <p className="text-xs text-muted-foreground">{t('pages.aiModel.thinking.prefixHint')}</p>
            </>}
        </>}
    </section>;
}
