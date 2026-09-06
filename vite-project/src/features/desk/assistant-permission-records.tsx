import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import type { PermissionRequestDto } from '@/services/types';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export function AssistantPermissionRecords({ requests, children, open, onOpenChange }: {
    requests: PermissionRequestDto[];
    children: (request: PermissionRequestDto) => ReactNode;
    open: boolean;
    onOpenChange: (open: boolean) => void;
}) {
    const { t } = useTranslation();
    const pending = requests.filter(request => ['pending', 'needs_revalidation'].includes(request.state));
    const history = requests.filter(request => !['pending', 'needs_revalidation'].includes(request.state));
    return <>
        {pending.length > 0 && <div data-testid="device-assistant-permission-requests" className="space-y-3 rounded-md border border-amber-500/40 p-3">
            <p className="text-sm font-medium">{t('pages.deviceAssistant.permissionTitle')}</p>
            <p className="text-xs text-muted-foreground">{t('pages.deviceAssistant.permissionDescription')}</p>
            {pending.map(children)}
        </div>}
        <Sheet open={open} onOpenChange={onOpenChange}>
            <SheetContent className="flex w-full flex-col overflow-hidden sm:max-w-xl">
                <SheetHeader>
                    <SheetTitle>{t('pages.deviceAssistant.permissionHistory')}</SheetTitle>
                    <SheetDescription>{t('pages.deviceAssistant.permissionHistoryHint')}</SheetDescription>
                </SheetHeader>
                <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-4">
                    {history.length ? [...history].reverse().map(children) : <p className="text-sm text-muted-foreground">{t('pages.deviceAssistant.permissionHistoryEmpty')}</p>}
                </div>
            </SheetContent>
        </Sheet>
    </>;
}
