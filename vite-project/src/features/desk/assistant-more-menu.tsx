import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { MoreHorizontal, type LucideIcon } from 'lucide-react';
import { useIsMobile } from '@/hooks/use-mobile';
import { Button } from '@/components/ui/button';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export type AssistantMoreSection = {
    label: string;
    actions: Array<{ label: string; icon?: LucideIcon; disabled?: boolean; onSelect: () => void }>;
};

export function AssistantMoreMenu({ sections }: { sections: AssistantMoreSection[] }) {
    const { t } = useTranslation();
    const isMobile = useIsMobile();
    const [open, setOpen] = useState(false);
    const label = t('pages.aiAssistant.workspace.more');
    const trigger = <Button type="button" variant="ghost" size="sm" className="assistant-action"
        aria-label={label} title={label}>
        <MoreHorizontal className="h-4 w-4 shrink-0" aria-hidden="true" />
        <span className="assistant-action-label">{label}</span>
    </Button>;

    if (isMobile) return <>
        <Button type="button" variant="ghost" size="sm" className="assistant-action"
            aria-label={label} title={label} onClick={() => setOpen(true)}>
            <MoreHorizontal className="h-4 w-4 shrink-0" aria-hidden="true" />
            <span className="assistant-action-label">{label}</span>
        </Button>
        <Sheet open={open} onOpenChange={setOpen}>
            <SheetContent side="bottom" className="max-h-[85dvh] overflow-y-auto rounded-t-xl px-4 pb-[calc(1rem+env(safe-area-inset-bottom))] pt-5">
                <SheetHeader><SheetTitle>{label}</SheetTitle></SheetHeader>
                <div className="mt-4 space-y-4">
                    {sections.map(section => <section key={section.label}>
                        <p className="mb-1 px-2 text-xs font-medium text-muted-foreground">{section.label}</p>
                        <div className="space-y-1">
                            {section.actions.map(action => <Button key={action.label} type="button" variant="ghost"
                                disabled={action.disabled} className="h-auto min-h-11 w-full justify-start gap-3 whitespace-normal text-left"
                                onClick={() => { setOpen(false); action.onSelect(); }}>
                                {action.icon && <action.icon className="h-4 w-4 shrink-0" aria-hidden="true" />}
                                {action.label}
                            </Button>)}
                        </div>
                    </section>)}
                </div>
            </SheetContent>
        </Sheet>
    </>;

    return <DropdownMenu>
        <DropdownMenuTrigger asChild>{trigger}</DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-56">
            {sections.map((section, index) => <div key={section.label}>
                {index > 0 && <DropdownMenuSeparator />}
                <DropdownMenuLabel>{section.label}</DropdownMenuLabel>
                {section.actions.map(action => <DropdownMenuItem key={action.label} disabled={action.disabled}
                    onSelect={action.onSelect}>
                    {action.icon && <action.icon aria-hidden="true" />}{action.label}
                </DropdownMenuItem>)}
            </div>)}
        </DropdownMenuContent>
    </DropdownMenu>;
}
