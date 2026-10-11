import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
    AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
    AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from '@/components/ui/alert-dialog';

interface ConfirmOptions {
    title: string;
    description?: string;
    confirmLabel?: string;
    cancelLabel?: string;
    destructive?: boolean;
}

/** Render `dialog` in the caller; only an explicit confirmation resolves true. */
export function useConfirm(labels?: Pick<ConfirmOptions, 'confirmLabel' | 'cancelLabel'>) {
    const { t } = useTranslation();
    const [options, setOptions] = useState<ConfirmOptions | null>(null);
    const pending = useRef<((confirmed: boolean) => void) | null>(null);
    const mounted = useRef(true);
    const returnFocus = useRef<HTMLElement | null>(null);

    useEffect(() => {
        mounted.current = true;
        return () => {
            mounted.current = false;
            pending.current?.(false);
            pending.current = null;
        };
    }, []);

    const confirm = useCallback((next: ConfirmOptions) => {
        if (!mounted.current || pending.current) return Promise.resolve(false);
        return new Promise<boolean>(resolve => {
            pending.current = resolve;
            returnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
            setOptions(next);
        });
    }, []);

    const settle = useCallback((confirmed: boolean) => {
        const resolve = pending.current;
        pending.current = null;
        setOptions(null);
        resolve?.(confirmed);
    }, []);

    const dialog = <AlertDialog open={options !== null} onOpenChange={open => { if (!open) settle(false); }}>
        <AlertDialogContent onCloseAutoFocus={event => {
            event.preventDefault();
            if (returnFocus.current?.isConnected) returnFocus.current.focus();
            returnFocus.current = null;
        }}>
            <AlertDialogHeader>
                <AlertDialogTitle>{options?.title}</AlertDialogTitle>
                <AlertDialogDescription className={options?.description ? undefined : 'sr-only'}>
                    {options?.description ?? options?.title}
                </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
                <AlertDialogCancel onClick={() => settle(false)}>
                    {options?.cancelLabel ?? labels?.cancelLabel ?? t('common.cancel')}
                </AlertDialogCancel>
                <AlertDialogAction onClick={() => settle(true)}
                    className={options?.destructive !== false ? 'bg-destructive text-destructive-foreground hover:bg-destructive/90' : undefined}>
                    {options?.confirmLabel ?? labels?.confirmLabel ?? t('common.confirm')}
                </AlertDialogAction>
            </AlertDialogFooter>
        </AlertDialogContent>
    </AlertDialog>;

    return { confirm, dialog, confirming: options !== null };
}
