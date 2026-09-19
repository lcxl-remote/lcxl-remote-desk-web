import { useTranslation } from 'react-i18next';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Switch } from '@/components/ui/switch';

export function PromptCacheSettings({ value, onChange }: { value: string; onChange: (value: string) => void }) {
    const { t } = useTranslation();
    let options: Record<string, unknown>;
    try {
        options = JSON.parse(value);
        if (!options || Array.isArray(options) || typeof options !== 'object') return null;
    } catch { return null; }
    const cache = options.prompt_cache as { mode?: string; cache_history?: boolean } | undefined;
    const mode = cache?.mode ?? 'provider_default';
    const update = (mode: string, history: boolean) => {
        const next = { ...options };
        if (mode === 'provider_default') delete next.prompt_cache;
        else next.prompt_cache = { mode, cache_history: history };
        onChange(JSON.stringify(next, null, 2));
    };
    return <div className="space-y-2 rounded-md border p-3">
        <label className="text-sm font-medium">{t('pages.aiModel.cache.label')}</label>
        <Select value={mode} onValueChange={value => update(value, false)}>
            <SelectTrigger aria-label={t('pages.aiModel.cache.label')}><SelectValue /></SelectTrigger>
            <SelectContent>
                <SelectItem value="provider_default">{t('pages.aiModel.cache.default')}</SelectItem>
                <SelectItem value="anthropic_explicit">{t('pages.aiModel.cache.explicit')}</SelectItem>
            </SelectContent>
        </Select>
        {mode === 'anthropic_explicit' && <label className="flex items-center gap-2 text-sm">
            <Switch checked={cache?.cache_history === true} onCheckedChange={value => update(mode, value)} />
            {t('pages.aiModel.cache.history')}
        </label>}
        <p className="text-xs text-muted-foreground">{t('pages.aiModel.cache.help')}</p>
    </div>;
}
