import type { ComponentProps, ReactNode } from 'react';
import { Select, SelectContent, SelectTrigger, SelectValue } from './select';

export function SelectField({ value, onValueChange, children, disabled, ...triggerProps }: {
    value: string | number; onValueChange: (value: string) => void; children: ReactNode;
} & Omit<ComponentProps<typeof SelectTrigger>, 'value' | 'onChange' | 'children'>) {
    return <Select value={String(value) || '__empty__'} onValueChange={next => onValueChange(next === '__empty__' ? '' : next)} disabled={disabled}>
        <SelectTrigger {...triggerProps}><SelectValue /></SelectTrigger>
        <SelectContent>{children}</SelectContent>
    </Select>;
}
