import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Disclosure } from '@/components/ui/disclosure';
import type { AssistantDirectoryOperation, AssistantFileScopeView } from './assistant-file-scope';

type Directory = AssistantFileScopeView['directories'][number];

export function AssistantDirectoryApproval({ directory, revision, disabled, busy, onUpdate }: {
    directory: Directory;
    revision: number;
    disabled: boolean;
    busy: boolean;
    onUpdate: (operation: AssistantDirectoryOperation, timeoutMessage: string) => boolean;
}) {
    const { t } = useTranslation();
    const [submitError, setSubmitError] = useState(false);
    const decide = (approve: boolean) => {
        setSubmitError(false);
        if (!onUpdate({ kind: 'decide_directory', directory_request_id: directory.requestId,
            approve, expected_revision: revision }, t('pages.aiAssistant.directories.timeout'))) {
            setSubmitError(true);
        }
    };
    return <section id={`assistant-directory-${directory.requestId}`} data-testid="assistant-directory-approval"
        className="space-y-3 rounded-md border border-amber-500/50 bg-amber-500/5 p-3 [overflow-wrap:anywhere]">
        <div>
            <p className="font-medium">{t('pages.aiAssistant.directories.requestTitle')}</p>
            <p className="mt-1 text-sm text-muted-foreground">{t('pages.aiAssistant.directories.currentSession')}</p>
        </div>
        <p className="break-all font-mono text-sm">{directory.canonicalPath}</p>
        <p className="whitespace-pre-wrap text-sm">{directory.purpose}</p>
        <Disclosure title={t('pages.aiAssistant.directories.requestDetails')} summaryClassName="cursor-pointer text-xs">
            <dl className="mt-2 space-y-1 break-all text-xs text-muted-foreground">
                <div><dt className="inline">ID: </dt><dd className="inline">{directory.requestId}</dd></div>
                <div><dt className="inline">rev: </dt><dd className="inline">{revision}</dd></div>
                <div><dt className="inline">{t('pages.aiAssistant.directories.referenceExpires')}: </dt>
                    <dd className="inline">{Number.isNaN(Date.parse(directory.referenceExpiresAt))
                        ? directory.referenceExpiresAt : new Date(directory.referenceExpiresAt).toLocaleString()}</dd></div>
            </dl>
        </Disclosure>
        {submitError && <p role="alert" className="text-sm text-destructive">{t('pages.aiAssistant.directories.submitUnavailable')}</p>}
        <div className="flex flex-wrap gap-2">
            <Button type="button" size="sm" disabled={disabled || busy} onClick={() => decide(true)}>
                {t('pages.aiAssistant.directories.approve')}
            </Button>
            <Button type="button" size="sm" variant="outline" disabled={disabled || busy} onClick={() => decide(false)}>
                {t('pages.aiAssistant.directories.reject')}
            </Button>
        </div>
    </section>;
}
