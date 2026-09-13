import type { ComponentProps, ReactNode } from 'react';
import { ChevronDown } from 'lucide-react';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from './collapsible';
import { Button } from './button';
import { cn } from '@/lib/utils';

export function Disclosure({ title, summaryClassName, children, ...props }: Omit<ComponentProps<typeof Collapsible>, 'title'> & {
    title: ReactNode; summaryClassName?: string;
}) {
    return <Collapsible data-slot="disclosure" {...props}>
        <CollapsibleTrigger asChild>
            <Button type="button" variant="unstyled" className={cn('group flex w-full min-w-0 items-start gap-2 text-left', summaryClassName)}>
                <ChevronDown aria-hidden="true" className="mt-0.5 h-4 w-4 shrink-0 -rotate-90 transition-transform group-data-[state=open]:rotate-0" />
                <span className="min-w-0 flex-1">{title}</span>
            </Button>
        </CollapsibleTrigger>
        <CollapsibleContent forceMount className="data-[state=closed]:hidden">{children}</CollapsibleContent>
    </Collapsible>;
}
