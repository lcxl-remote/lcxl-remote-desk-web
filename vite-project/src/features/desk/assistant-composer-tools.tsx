import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { ClipboardList, ListTree, FolderKey } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AssistantComposerTools({ meter, onDetails, onPermissionHistory, onDirectories }: {
    meter: ReactNode;
    onDetails: () => void;
    onPermissionHistory: () => void;
    onDirectories?: () => void;
}) {
    const { t } = useTranslation();
    return <div className="flex min-w-0 items-center gap-1" data-testid="assistant-composer-tools">
        {meter}
        <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.workspace.details')}
            aria-label={t('pages.deviceAssistant.workspace.details')} aria-haspopup="dialog" onClick={onDetails}>
            <ListTree className="h-4 w-4" aria-hidden="true" />
        </Button>
        <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.permissionHistory')}
            aria-label={t('pages.deviceAssistant.permissionHistory')} aria-haspopup="dialog" onClick={onPermissionHistory}>
            <ClipboardList className="h-4 w-4" aria-hidden="true" />
        </Button>
        {onDirectories && <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.directories.title')}
            aria-label={t('pages.deviceAssistant.directories.title')} aria-haspopup="dialog" onClick={onDirectories}>
            <FolderKey className="h-4 w-4" aria-hidden="true" />
        </Button>}
    </div>;
}
