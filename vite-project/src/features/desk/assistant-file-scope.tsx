import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { RemoteDirectoryPicker } from '@/features/file-manager/remote-directory-picker';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export type AssistantFileScopeView = {
    revision: number;
    directories: {
        requestId: string;
        canonicalPath: string;
        purpose: string;
        state: string;
        source: string;
        referenceExpiresAt: string;
    }[];
};

export type AssistantDirectoryOperation =
    | { kind: 'select_directory'; path: string; purpose: string; expected_revision: number }
    | { kind: 'decide_directory'; directory_request_id: string; approve: boolean; expected_revision: number }
    | { kind: 'revoke_directory'; directory_request_id: string; expected_revision: number };

export function AssistantFileScope({ scope, open, onOpenChange, disabled, onUpdate, deskId, sessionTargetId }: {
    deskId?: string;
    // undefined: unresolved; null: selected anonymous in-process worker.
    sessionTargetId?: string | null;
    scope: AssistantFileScopeView;
    open: boolean;
    onOpenChange: (open: boolean) => void;
    disabled: boolean;
    onUpdate: (operation: AssistantDirectoryOperation, timeoutMessage: string) => boolean;
}) {
    const { t } = useTranslation();
    const [browsing, setBrowsing] = useState(false);
    const submit = (operation: AssistantDirectoryOperation) => onUpdate(operation, t('pages.aiAssistant.directories.timeout'));
    return <Sheet open={open} onOpenChange={onOpenChange}>
        <SheetContent className="flex w-full flex-col gap-4 overflow-y-auto sm:max-w-lg">
            <SheetHeader>
                <SheetTitle>{t('pages.aiAssistant.directories.title')}</SheetTitle>
                <SheetDescription>{t('pages.aiAssistant.directories.hint')}</SheetDescription>
            </SheetHeader>
            <div className="space-y-2 px-4">
                <Button type="button" disabled={disabled || !deskId || sessionTargetId === undefined} onClick={() => setBrowsing(value => !value)}>
                    {t('pages.aiAssistant.directories.add')}
                </Button>
                {open && browsing && deskId && sessionTargetId !== undefined && <RemoteDirectoryPicker
                    key={`${deskId}:${sessionTargetId}`} deskId={deskId} sessionTargetId={sessionTargetId}
                    disabled={disabled} onSelect={path => submit({ kind: 'select_directory', path,
                        purpose: t('pages.aiAssistant.directories.manualPurpose'), expected_revision: scope.revision })}
                    onCancel={() => setBrowsing(false)} />}
            </div>
            <div className="space-y-3 px-4 pb-4">
                {!scope.directories.length && <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.directories.empty')}</p>}
                {scope.directories.map(directory => <div key={directory.requestId} className="space-y-2 rounded-md border p-3">
                    <p className="break-all font-mono text-sm">{directory.canonicalPath}</p>
                    <p className="break-words text-xs text-muted-foreground">{directory.purpose}</p>
                    <p className="text-xs">{directory.state === 'pending' ? t('pages.aiAssistant.directories.pending')
                        : directory.state === 'approved' ? t('pages.aiAssistant.directories.approved') : directory.state}</p>
                    <div className="flex gap-2">
                        {directory.state === 'pending' ? <>
                            <Button type="button" size="sm" disabled={disabled} onClick={() => {
                                submit({ kind: 'decide_directory', directory_request_id: directory.requestId, approve: true, expected_revision: scope.revision });
                            }}>{t('pages.aiAssistant.directories.approve')}</Button>
                            <Button type="button" size="sm" variant="outline" disabled={disabled} onClick={() => submit({ kind: 'decide_directory', directory_request_id: directory.requestId, approve: false, expected_revision: scope.revision })}>{t('pages.aiAssistant.directories.reject')}</Button>
                        </> : directory.state === 'approved' && <Button type="button" size="sm" variant="outline" disabled={disabled} onClick={() => submit({ kind: 'revoke_directory', directory_request_id: directory.requestId, expected_revision: scope.revision })}>{t('pages.aiAssistant.directories.remove')}</Button>}
                    </div>
                </div>)}
            </div>
        </SheetContent>
    </Sheet>;
}
