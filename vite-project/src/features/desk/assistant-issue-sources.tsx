import { useCallback, useEffect, useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RefreshCw, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { getMyIssueConnections } from '@/services/clients';
import type { IssueConnectionChoice, IssueSelection } from '@/services/types';

export function parseIssueUrl(connection: IssueConnectionChoice, value: string): IssueSelection | null {
    try {
        const base = new URL(connection.instance_url);
        const url = new URL(value.trim());
        const prefix = `${base.pathname.replace(/\/$/, '')}/`;
        if (url.origin !== base.origin || url.protocol !== 'https:' || url.username || url.password
            || !url.pathname.startsWith(prefix)) return null;
        const [owner, repository, kind, number, ...extra] = url.pathname.slice(prefix.length)
            .replace(/\/$/, '').split('/').map(decodeURIComponent);
        const valid = (part: string) => !!part && part.length <= 256 && part !== '.' && part !== '..'
            && /^[a-zA-Z0-9_.-]+$/.test(part);
        if (extra.length || !valid(owner) || !valid(repository) || kind !== 'issues'
            || !/^[1-9]\d*$/.test(number)) return null;
        const issueNumber = Number(number);
        if (!Number.isSafeInteger(issueNumber)) return null;
        return { connection_ref: connection.connection_ref, owner, repository, issue_number: issueNumber };
    } catch { return null; }
}

export function AssistantIssueSources({ value, onChange, disabled }: {
    value: IssueSelection[];
    onChange(value: IssueSelection[]): void;
    disabled: boolean;
}) {
    const { t } = useTranslation();
    const id = useId();
    const [connections, setConnections] = useState<IssueConnectionChoice[]>([]);
    const [connection, setConnection] = useState(value[0]?.connection_ref ?? '');
    const [url, setUrl] = useState('');
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    const [reload, setReload] = useState(0);
    useEffect(() => {
        const controller = new AbortController();
        setLoading(true); setError('');
        void getMyIssueConnections({ signal: controller.signal }).then(response => {
            if (controller.signal.aborted) return;
            if (!response.success || !Array.isArray(response.data)) throw new Error('Issue connections unavailable');
            const choices = response.data;
            setConnections(choices);
            setConnection(current => choices.some(choice => choice.connection_ref === current)
                ? current : choices[0]?.connection_ref ?? '');
        }).catch(() => {
            if (!controller.signal.aborted) { setConnections([]); setError('loadFailed'); }
        }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
        return () => controller.abort();
    }, [reload]);
    const add = useCallback(() => {
        if (disabled || loading) return;
        const selected = connections.find(item => item.connection_ref === connection);
        const issue = selected ? parseIssueUrl(selected, url) : null;
        if (!issue) { setError('invalid'); return; }
        if (value.some(existing => existing.connection_ref === issue.connection_ref
            && existing.owner.toLowerCase() === issue.owner.toLowerCase()
            && existing.repository.toLowerCase() === issue.repository.toLowerCase()
            && existing.issue_number === issue.issue_number)) {
            setError('duplicate'); return;
        }
        onChange([...value, issue]); setUrl(''); setError('');
    }, [connection, connections, disabled, loading, onChange, url, value]);
    return <Card>
        <CardHeader className="flex-row items-center justify-between gap-2">
            <CardTitle className="text-base">{t('pages.aiAssistant.issueSources.title')}</CardTitle>
            <Button type="button" size="icon" variant="ghost" disabled={loading || disabled}
                aria-label={t('pages.aiAssistant.issueSources.reload')} onClick={() => setReload(previous => previous + 1)}>
                <RefreshCw className="h-4 w-4" />
            </Button>
        </CardHeader>
        <CardContent className="space-y-3">
            {value.map((issue, index) => {
                const label = `${issue.owner}/${issue.repository} #${issue.issue_number}`;
                const name = connections.find(item => item.connection_ref === issue.connection_ref)?.name;
                return <div key={`${issue.connection_ref}:${issue.owner}:${issue.repository}:${issue.issue_number}`}
                    className="flex items-center justify-between gap-2 rounded-md bg-muted px-3 py-2">
                    <span className="min-w-0 break-words text-sm">{name ? `${name} · ` : ''}{label}</span>
                    <Button type="button" size="icon" variant="ghost" disabled={disabled}
                        aria-label={t('pages.aiAssistant.issueSources.remove', { issue: label })}
                        onClick={() => onChange(value.filter((_, position) => position !== index))}>
                        <X className="h-4 w-4" />
                    </Button>
                </div>;
            })}
            {connections.length > 0 && <>
                <div className="space-y-1">
                    <Label htmlFor={`${id}-connection`}>{t('pages.aiAssistant.issueSources.connection')}</Label>
                    <Select value={connection} disabled={disabled || loading} onValueChange={next => { setConnection(next); setError(''); }}>
                        <SelectTrigger id={`${id}-connection`}><SelectValue /></SelectTrigger>
                        <SelectContent>{connections.map(choice => <SelectItem key={choice.connection_ref} value={choice.connection_ref}>{choice.name}</SelectItem>)}</SelectContent>
                    </Select>
                </div>
                <div className="space-y-1">
                    <Label htmlFor={`${id}-url`}>{t('pages.aiAssistant.issueSources.url')}</Label>
                    <Input id={`${id}-url`} type="url" value={url} disabled={disabled || loading}
                        onChange={event => { setUrl(event.target.value); setError(''); }}
                        onKeyDown={event => { if (event.key === 'Enter') { event.preventDefault(); add(); } }} />
                </div>
                <Button type="button" variant="outline" disabled={disabled || loading || !url.trim()} onClick={add}>
                    {t('pages.aiAssistant.issueSources.add')}
                </Button>
            </>}
            {!loading && !error && !connections.length && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.issueSources.empty')}</p>}
            {error && <p role="alert" className="text-sm text-destructive">{t(`pages.aiAssistant.issueSources.${error}`)}</p>}
        </CardContent>
    </Card>;
}
