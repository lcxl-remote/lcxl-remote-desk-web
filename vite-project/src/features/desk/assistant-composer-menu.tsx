import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Layers, Plus, ToggleLeft, ToggleRight, type LucideIcon } from 'lucide-react';
import { useIsMobile } from '@/hooks/use-mobile';
import { Button } from '@/components/ui/button';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';

export type AssistantMenuSection = {
    label: string;
    actions: Array<{ label: string; icon?: LucideIcon; disabled?: boolean; onSelect: () => void }>;
};

export function AssistantComposerMenu({ sections, onContext, goal }: {
    sections: AssistantMenuSection[];
    onContext?: () => void;
    goal?: { checked: boolean; disabled: boolean; onCheckedChange: (checked: boolean) => void; summary?: ReactNode };
}) {
    const { t } = useTranslation();
    const isMobile = useIsMobile();
    const [open, setOpen] = useState(false);
    const GoalIcon = goal?.checked ? ToggleRight : ToggleLeft;
    const label = t('pages.aiAssistant.workspace.actions');
    const trigger = <Button type="button" variant="ghost" size="icon" className="assistant-add-action h-11 w-11 shrink-0"
        aria-label={label} title={label}>
        <Plus className="h-4 w-4" aria-hidden="true" />
    </Button>;

    if (isMobile) return <>
        <Button type="button" variant="ghost" size="icon" className="assistant-add-action h-11 w-11 shrink-0"
            aria-label={label} title={label} onClick={() => setOpen(true)}>
            <Plus className="h-4 w-4" aria-hidden="true" />
        </Button>
        <Sheet open={open} onOpenChange={setOpen}>
            <SheetContent side="bottom" aria-describedby={undefined} className="max-h-[85dvh] overflow-y-auto rounded-t-xl px-4 pb-[calc(1rem+env(safe-area-inset-bottom))] pt-5">
                <SheetHeader><SheetTitle>{label}</SheetTitle></SheetHeader>
                <div className="mt-4 space-y-4">
                    {(onContext || goal) && <div className="space-y-1">
                        {onContext && <Button type="button" variant="ghost" className="min-h-11 w-full justify-start gap-3"
                            onClick={() => { setOpen(false); onContext(); }}>
                            <Layers className="h-4 w-4 shrink-0" aria-hidden="true" />
                            {t('pages.aiAssistant.workspace.addContext')}
                        </Button>}
                        {goal && <Button type="button" variant="ghost" className="min-h-11 w-full justify-start gap-3"
                            aria-pressed={goal.checked} disabled={goal.disabled} onClick={() => goal.onCheckedChange(!goal.checked)}>
                            <GoalIcon className={`h-4 w-4 shrink-0${goal.checked ? ' text-primary' : ''}`} aria-hidden="true" />
                            {t('pages.aiAssistant.goalStart')}
                        </Button>}
                        {goal?.summary}
                    </div>}
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
        <DropdownMenuContent align="start" side="top" className="min-w-56 max-w-[min(90vw,20rem)]">
            {onContext && <DropdownMenuItem onSelect={onContext}>
                <Layers aria-hidden="true" />{t('pages.aiAssistant.workspace.addContext')}
            </DropdownMenuItem>}
            {goal && <DropdownMenuItem role="menuitemcheckbox" aria-checked={goal.checked} disabled={goal.disabled}
                onSelect={() => goal.onCheckedChange(!goal.checked)}>
                <GoalIcon className={goal.checked ? 'text-primary' : undefined} aria-hidden="true" />{t('pages.aiAssistant.goalStart')}
            </DropdownMenuItem>}
            {goal?.summary && <div className="px-2 py-1">{goal.summary}</div>}
            {sections.map((section, index) => <div key={section.label}>
                {(index > 0 || onContext || goal) && <DropdownMenuSeparator />}
                <DropdownMenuLabel>{section.label}</DropdownMenuLabel>
                {section.actions.map(action => <DropdownMenuItem key={action.label} disabled={action.disabled}
                    onSelect={action.onSelect}>
                    {action.icon && <action.icon aria-hidden="true" />}{action.label}
                </DropdownMenuItem>)}
            </div>)}
        </DropdownMenuContent>
    </DropdownMenu>;
}
