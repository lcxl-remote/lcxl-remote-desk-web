import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export type AssistantPanelId = 'details' | 'capabilities' | 'context' | 'connection' | 'observation';

export function AssistantDetailsSheet({ panel, onPanelChange, sections }: {
    panel: AssistantPanelId | null;
    onPanelChange: (panel: AssistantPanelId | null) => void;
    sections: Record<AssistantPanelId, ReactNode>;
}) {
    const { t } = useTranslation();
    return (
        <Sheet open={panel !== null} onOpenChange={(open) => { if (!open) onPanelChange(null); }}>
            <SheetContent className="flex w-full flex-col overflow-hidden sm:max-w-xl">
                <SheetHeader className="pr-8 text-left">
                    <SheetTitle>{t(`pages.deviceAssistant.workspace.${panel ?? 'details'}`)}</SheetTitle>
                    <SheetDescription>{t('pages.deviceAssistant.workspace.panelHint')}</SheetDescription>
                </SheetHeader>
                <nav className="flex flex-wrap gap-1 border-b py-3" aria-label={t('pages.deviceAssistant.workspace.details')}>
                    {(['details', 'capabilities', 'context', 'connection', 'observation'] as const).map((id) => (
                        <Button key={id} type="button" size="sm" variant={panel === id ? 'secondary' : 'ghost'}
                            aria-current={panel === id ? 'page' : undefined} onClick={() => onPanelChange(id)}>
                            {t(`pages.deviceAssistant.workspace.${id}`)}
                        </Button>
                    ))}
                </nav>
                <div className="min-h-0 flex-1 space-y-4 overflow-y-auto overscroll-contain py-4 [overflow-wrap:anywhere]">
                    {panel && sections[panel]}
                </div>
            </SheetContent>
        </Sheet>
    );
}
