import type { ReactNode } from 'react';
import { LoaderCircle } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useTranslation } from 'react-i18next';
import type { PermissionRequestDto } from '@/services/types';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export function permissionIsAutomaticallyReviewed(request: PermissionRequestDto, automaticApproval: boolean) {
    return automaticApproval && request.state === 'pending'
        && !request.items.some(item => Boolean(item.commandConfirmation?.interactive));
}

export function AssistantPermissionRecords({ requests, children, open, onOpenChange, automaticApproval = false }: {
    requests: PermissionRequestDto[];
    automaticApproval?: boolean;
    children: (request: PermissionRequestDto) => ReactNode;
    open: boolean;
    onOpenChange: (open: boolean) => void;
}) {
    const { t } = useTranslation();
    const pending = requests.filter(request => ['pending', 'needs_revalidation'].includes(request.state));
    const reviewing = pending.filter(request => permissionIsAutomaticallyReviewed(request, automaticApproval));
    const manual = pending.filter(request => !permissionIsAutomaticallyReviewed(request, automaticApproval));
    const latest = requests.at(-1);
    const reviewFailed = latest?.decision?.source === 'review_unavailable';
    const history = requests.filter(request => !['pending', 'needs_revalidation'].includes(request.state));
    return <>
        {reviewing.length > 0 && <div role="status" data-testid="ai-assistant-automatic-review" className="flex items-center gap-2 px-1 text-xs text-muted-foreground">
            <LoaderCircle className="h-4 w-4 shrink-0 animate-spin" aria-hidden="true" />
            {t('pages.aiAssistant.permissionAutomaticReview')}
        </div>}
        {reviewFailed && <div role="alert" className="space-y-2 rounded-md border border-destructive/40 p-3 text-sm text-destructive">
            <p>{t('pages.aiAssistant.permissionDecisionSource.review_unavailable')}</p>
            <Button type="button" size="sm" variant="outline" onClick={() => onOpenChange(true)}>
                {t('pages.aiAssistant.permissionHistory')}
            </Button>
        </div>}
        {manual.length > 0 && <div data-testid="ai-assistant-permission-requests" className="space-y-3 rounded-md border border-amber-500/40 p-3">
            <p className="text-sm font-medium">{t('pages.aiAssistant.permissionTitle')}</p>
            <p className="text-xs text-muted-foreground">{t('pages.aiAssistant.permissionDescription')}</p>
            {manual.map(children)}
        </div>}
        <Sheet open={open} onOpenChange={onOpenChange}>
            <SheetContent className="flex w-full flex-col overflow-hidden sm:max-w-xl">
                <SheetHeader>
                    <SheetTitle>{t('pages.aiAssistant.permissionHistory')}</SheetTitle>
                    <SheetDescription>{t('pages.aiAssistant.permissionHistoryHint')}</SheetDescription>
                </SheetHeader>
                <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-4">
                    {history.length ? [...history].reverse().map(children) : <p className="text-sm text-muted-foreground">{t('pages.aiAssistant.permissionHistoryEmpty')}</p>}
                </div>
            </SheetContent>
        </Sheet>
    </>;
}
