import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { ClipboardList, ListTree, FolderKey, ListTodo } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AssistantComposerTools({ meter, onDetails, onPermissionHistory, onDirectories, onTasks, runningTaskCount = 0 }: {
    meter: ReactNode;
    onTasks?: () => void;
    runningTaskCount?: number;
    onDetails: () => void;
    onPermissionHistory: () => void;
    onDirectories?: () => void;
}) {
    const { t } = useTranslation();
    return <div className="flex min-w-0 flex-wrap items-center gap-1" data-testid="assistant-composer-tools">
        {meter}
        <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.workspace.details')}
            aria-label={t('pages.deviceAssistant.workspace.details')} aria-haspopup="dialog" onClick={onDetails}>
            <ListTree className="h-4 w-4" aria-hidden="true" />
        </Button>
        <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.permissionHistory')}
            aria-label={t('pages.deviceAssistant.permissionHistory')} aria-haspopup="dialog" onClick={onPermissionHistory}>
            <ClipboardList className="h-4 w-4" aria-hidden="true" />
        </Button>
        {onTasks && <Button type="button" size="sm" variant="ghost" title={t('pages.deviceAssistant.tasks.title')}
            aria-label={t('pages.deviceAssistant.tasks.title')} aria-haspopup="dialog" onClick={onTasks}>
            <ListTodo className="mr-1 h-4 w-4" aria-hidden="true" />
            <span>{t('pages.deviceAssistant.tasks.title')}</span>
            {runningTaskCount > 0 && <span className="ml-1 tabular-nums">({runningTaskCount})</span>}
        </Button>}
        {onDirectories && <Button type="button" size="icon" variant="ghost" title={t('pages.deviceAssistant.directories.title')}
            aria-label={t('pages.deviceAssistant.directories.title')} aria-haspopup="dialog" onClick={onDirectories}>
            <FolderKey className="h-4 w-4" aria-hidden="true" />
        </Button>}
    </div>;
}
