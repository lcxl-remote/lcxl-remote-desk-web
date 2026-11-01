import { useCallback, useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import type { AssistantDataSettingsView, UserInstructions } from '@/services/types';
import { AiAssistantHubLayout } from './ai-assistant-hub-layout';
import { assistantSettingsErrorKey, getAssistantDataSettings, getAssistantPersonalization,
    saveAssistantDataSettings, saveAssistantPersonalization,
    validPersonalization } from './assistant-data-api';

const prefix = 'pages.aiAssistant.personalization.';

// Server revisions remain the authority. An uncertain write is never retried
// with a fresh revision automatically; only an explicit reload replaces drafts.
function usePreference<T>(read: () => Promise<T>, write: (base: T, draft: T) => Promise<T>) {
    const [base, setBase] = useState<T | null>(null);
    const [draft, setDraft] = useState<T | null>(null);
    const [busy, setBusy] = useState<'load' | 'save' | null>('load');
    const [error, setError] = useState('');
    const [saved, setSaved] = useState(false);
    const epoch = useRef(0);
    const posting = useRef(false);
    const reload = useCallback(async () => {
        if (posting.current) return;
        const own = ++epoch.current;
        setBusy('load'); setError(''); setSaved(false);
        try {
            const next = await read();
            if (own !== epoch.current) return;
            setBase(next); setDraft(next);
        } catch (cause) { if (own === epoch.current) setError(assistantSettingsErrorKey(cause)); }
        finally { if (own === epoch.current) setBusy(null); }
    }, [read]);
    useEffect(() => { void reload(); return () => { epoch.current += 1; }; }, [reload]);
    const commit = async () => {
        if (!base || !draft || posting.current || busy) return;
        posting.current = true;
        const own = ++epoch.current;
        setBusy('save'); setError(''); setSaved(false);
        try {
            const next = await write(base, draft);
            if (own !== epoch.current) return;
            setBase(next); setDraft(next); setSaved(true);
        } catch (cause) { if (own === epoch.current) setError(assistantSettingsErrorKey(cause)); }
        finally { posting.current = false; if (own === epoch.current) setBusy(null); }
    };
    const edit = (next: T) => { setDraft(next); setSaved(false); };
    return { base, draft, edit, busy, error, saved, reload, commit,
        dirty: base != null && JSON.stringify(base) !== JSON.stringify(draft) };
}

const writeInstructions = (base: UserInstructions, draft: UserInstructions) => saveAssistantPersonalization({
    expected_revision: base.revision, content: draft.content,
});
const writeSettings = (base: AssistantDataSettingsView, draft: AssistantDataSettingsView) => saveAssistantDataSettings({
    expected_revision: base.settings.revision,
    automatic_memory_enabled: draft.settings.automatic_memory_enabled,
    model_history_search_enabled: draft.settings.model_history_search_enabled,
    explanation_version: Math.max(1, draft.settings.explanation_version),
});

function PersonalInstructions({ maximumBytes }: { maximumBytes?: number }) {
    const { t } = useTranslation();
    const value = usePreference(getAssistantPersonalization, writeInstructions);
    const bytes = new TextEncoder().encode(value.draft?.content ?? '').byteLength;
    const valid = value.draft != null && maximumBytes != null && validPersonalization(value.draft.content, maximumBytes);
    return <Card><CardHeader>
        <CardTitle>{t(`${prefix}instructionsTitle`)}</CardTitle>
    </CardHeader><CardContent className="space-y-4">
        {value.draft && <>
            <Label className="sr-only" htmlFor="assistant-personal-instructions">{t(`${prefix}instructionsLabel`)}</Label>
            <Textarea id="assistant-personal-instructions" rows={6} value={value.draft.content} disabled={!!value.busy}
                aria-describedby="assistant-personal-instructions-help" aria-invalid={!valid}
                placeholder={t(`${prefix}instructionsPlaceholder`)}
                onChange={event => value.edit({ ...value.draft!, content: event.target.value })} />
            <p id="assistant-personal-instructions-help" className="text-sm text-muted-foreground">
                {t(`${prefix}bytes`, { bytes, maximum: maximumBytes ?? 0 })}
            </p>
            {!valid && <p role="alert" className="text-sm text-destructive">{t(`${prefix}tooLong`)}</p>}
        </>}
        {value.error && <p role="alert" className="text-sm text-destructive">{t(value.error)}</p>}
        {value.saved && <p role="status" className="text-sm">{t(`${prefix}instructionsSaved`)}</p>}
        {value.busy === 'load' && <p role="status">{t(`${prefix}loading`)}</p>}
        <div className="flex flex-wrap gap-2">
            <Button disabled={!!value.busy || !value.dirty || !valid} onClick={() => { void value.commit(); }}>
                {t(`${prefix}saveInstructions`)}
            </Button>
            <Button variant="outline" disabled={!!value.busy} onClick={() => { void value.reload(); }}>{t(`${prefix}reload`)}</Button>
        </div>
    </CardContent></Card>;
}

function AssistantDataPreferences({ value }: { value: ReturnType<typeof usePreference<AssistantDataSettingsView>> }) {
    const { t } = useTranslation();
    const draft = value.draft;
    const valid = draft != null;
    const edit = (next: Partial<AssistantDataSettingsView['settings']>) => {
        if (draft) value.edit({ ...draft, settings: { ...draft.settings, ...next } });
    };
    return <Card><CardHeader>
        <CardTitle>{t(`${prefix}dataTitle`)}</CardTitle>
    </CardHeader><CardContent className="space-y-5">
        {draft && <>
            <div className="flex items-center justify-between gap-4">
                <Label htmlFor="assistant-automatic-memory">{t(`${prefix}automaticMemory`)}</Label>
                <Switch id="assistant-automatic-memory" checked={draft.settings.automatic_memory_enabled} disabled={!!value.busy}
                    onCheckedChange={checked => edit({ automatic_memory_enabled: checked })} />
            </div>
            <div className="flex items-start justify-between gap-4">
                <div className="space-y-1"><Label htmlFor="assistant-history-search">{t(`${prefix}historySearch`)}</Label>
                    <p id="assistant-history-search-help" className="text-sm text-muted-foreground">{t(`${prefix}historySearchDescription`, { days: draft.policy.history_retention_days })}</p></div>
                <Switch id="assistant-history-search" checked={draft.settings.model_history_search_enabled} disabled={!!value.busy}
                    aria-describedby="assistant-history-search-help" onCheckedChange={checked => edit({ model_history_search_enabled: checked })} />
            </div>
        </>}
        {value.error && <p role="alert" className="text-sm text-destructive">{t(value.error)}</p>}
        {value.saved && <p role="status" className="text-sm">{t(`${prefix}settingsSaved`)}</p>}
        {value.busy === 'load' && <p role="status">{t(`${prefix}loading`)}</p>}
        <div className="flex flex-wrap gap-2">
            <Button disabled={!!value.busy || !value.dirty || !valid} onClick={() => { void value.commit(); }}>{t(`${prefix}saveSettings`)}</Button>
            <Button variant="outline" disabled={!!value.busy} onClick={() => { void value.reload(); }}>{t(`${prefix}reload`)}</Button>
            <Button variant="link" asChild><Link to="/ai-assistant/memories">{t(`${prefix}manageMemories`)}</Link></Button>
        </div>
    </CardContent></Card>;
}

export default function AiAssistantPersonalization() {
    const { t } = useTranslation();
    const preferences = usePreference(getAssistantDataSettings, writeSettings);
    return <AiAssistantHubLayout>
        <h1 className="text-2xl font-semibold">{t(`${prefix}title`)}</h1>
        <PersonalInstructions maximumBytes={preferences.base?.policy.limits?.memory?.user_instructions_bytes} />
        <AssistantDataPreferences value={preferences} />
    </AiAssistantHubLayout>;
}
