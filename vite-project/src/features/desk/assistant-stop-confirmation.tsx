import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';

export function AssistantStopConfirmation({ open, busy, onDismiss, onConfirm }: {
    open: boolean; busy: boolean; onDismiss: () => void; onConfirm: (include: boolean) => Promise<boolean>;
}) {
    const { t } = useTranslation();
    const [include, setInclude] = useState(true);
    useEffect(() => { if (open) setInclude(true); }, [open]);
    return <Dialog open={open} onOpenChange={next => { if (!next && !busy) onDismiss(); }}>
        <DialogContent>
            <DialogHeader>
                <DialogTitle>{t('pages.aiAssistant.subagents.stopTitle')}</DialogTitle>
                <DialogDescription>{t('pages.aiAssistant.subagents.stopDescription')}</DialogDescription>
            </DialogHeader>
            <label className="flex cursor-pointer items-start gap-3 text-sm">
                <Checkbox checked={include} disabled={busy} onCheckedChange={value => setInclude(value === true)} />
                <span>{t('pages.aiAssistant.subagents.stopInclude')}</span>
            </label>
            <DialogFooter>
                <Button type="button" variant="outline" disabled={busy} onClick={onDismiss}>{t('pages.aiAssistant.subagents.keepRunning')}</Button>
                <Button type="button" disabled={busy} onClick={() => void onConfirm(include)}>
                    {t(busy ? 'pages.aiAssistant.stopping' : 'pages.aiAssistant.stop')}
                </Button>
            </DialogFooter>
        </DialogContent>
    </Dialog>;
}
