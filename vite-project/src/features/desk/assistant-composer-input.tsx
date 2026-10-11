import { forwardRef, useEffect, useId, useRef, useState, type ComponentProps } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, Keyboard } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { DropdownMenu, DropdownMenuContent, DropdownMenuLabel, DropdownMenuRadioGroup, DropdownMenuRadioItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';

const STORAGE_KEY = 'ai-assistant-send-shortcut';
const CHANGE_EVENT = 'ai-assistant-send-shortcut-changed';
const shortcuts = ['enter', 'shift-enter', 'ctrl-enter'] as const;
type SendShortcut = typeof shortcuts[number];
const shortcutKeys = {
    enter: 'pages.aiAssistant.sendShortcut.enter',
    'shift-enter': 'pages.aiAssistant.sendShortcut.shiftEnter',
    'ctrl-enter': 'pages.aiAssistant.sendShortcut.ctrlEnter',
} as const;

function isSendShortcut(value: unknown): value is SendShortcut {
    return shortcuts.some(shortcut => shortcut === value);
}

function readShortcut(): SendShortcut {
    try {
        const saved = localStorage.getItem(STORAGE_KEY);
        return isSendShortcut(saved) ? saved : 'enter';
    } catch {
        return 'enter';
    }
}

function useSendShortcut() {
    const [shortcut, setShortcut] = useState(readShortcut);
    useEffect(() => {
        const onStorage = (event: StorageEvent) => {
            if (event.key === STORAGE_KEY || event.key === null) setShortcut(readShortcut());
        };
        const onChange = (event: Event) => {
            const value = (event as CustomEvent<unknown>).detail;
            if (isSendShortcut(value)) setShortcut(value);
        };
        window.addEventListener('storage', onStorage);
        window.addEventListener(CHANGE_EVENT, onChange);
        return () => {
            window.removeEventListener('storage', onStorage);
            window.removeEventListener(CHANGE_EVENT, onChange);
        };
    }, []);
    const updateShortcut = (value: string) => {
        if (!isSendShortcut(value)) return;
        setShortcut(value);
        try {
            localStorage.setItem(STORAGE_KEY, value);
        } catch {
            // The current page can still use the preference when storage is blocked.
        }
        window.dispatchEvent(new CustomEvent(CHANGE_EVENT, { detail: value }));
    };
    return [shortcut, updateShortcut] as const;
}

type AssistantComposerInputProps = ComponentProps<typeof Textarea> & {
    sendDisabled: boolean;
    onSend?: () => void;
};

export const AssistantComposerInput = forwardRef<HTMLTextAreaElement, AssistantComposerInputProps>(
    ({ sendDisabled, onSend, onKeyDown, onCompositionStart, onCompositionEnd, ...props }, ref) => {
        const { t } = useTranslation();
        const [shortcut, setShortcut] = useSendShortcut();
        const composing = useRef(false);
        const hintId = useId();
        const hint = t(shortcutKeys[shortcut]);
        return <>
            <Textarea {...props} ref={ref}
                aria-describedby={[props['aria-describedby'], hintId].filter(Boolean).join(' ')}
                onCompositionStart={event => { composing.current = true; onCompositionStart?.(event); }}
                onCompositionEnd={event => { composing.current = false; onCompositionEnd?.(event); }}
                onKeyDown={event => {
                    onKeyDown?.(event);
                    if (event.defaultPrevented || event.key !== 'Enter' || composing.current
                        || event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229 || event.altKey) return;
                    const matches = shortcut === 'enter' ? !event.shiftKey && !event.ctrlKey && !event.metaKey
                        : shortcut === 'shift-enter' ? event.shiftKey && !event.ctrlKey && !event.metaKey
                            : !event.shiftKey && (event.ctrlKey || event.metaKey);
                    if (!matches) return;
                    event.preventDefault();
                    if (event.repeat || props.disabled || sendDisabled) return;
                    if (onSend) onSend();
                    else event.currentTarget.form?.requestSubmit();
                }} />
            <div className="flex min-w-0 justify-end">
                <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                        <Button type="button" variant="ghost" size="sm"
                            className="h-auto min-h-8 max-w-full gap-1.5 whitespace-normal py-1 text-left text-xs text-muted-foreground"
                            aria-label={t('pages.aiAssistant.sendShortcut.label')}>
                            <Keyboard className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                            <span id={hintId}>{hint}</span>
                            <ChevronDown className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
                        </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end" className="max-w-[calc(100vw-2rem)]">
                        <DropdownMenuLabel>{t('pages.aiAssistant.sendShortcut.label')}</DropdownMenuLabel>
                        <DropdownMenuRadioGroup value={shortcut} onValueChange={setShortcut}>
                            {shortcuts.map(value => <DropdownMenuRadioItem key={value} value={value}
                                className="whitespace-normal">{t(shortcutKeys[value])}</DropdownMenuRadioItem>)}
                        </DropdownMenuRadioGroup>
                    </DropdownMenuContent>
                </DropdownMenu>
            </div>
        </>;
    },
);
AssistantComposerInput.displayName = 'AssistantComposerInput';
