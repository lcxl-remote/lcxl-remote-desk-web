import { useTranslation } from 'react-i18next';
import type { GrantRequestItemDto, WaylandOutputConfirmationDto } from '@/services/types';

export function validOutputReview(value: WaylandOutputConfirmationDto | null | undefined): value is WaylandOutputConfirmationDto {
    if (value?.wholeOutput !== true || value.oneShot !== true || !value.screen?.display?.trim()) return false;
    const screen = value.screen;
    const integerIn = (n: number, low: number, high: number) => Number.isInteger(n) && n >= low && n <= high;
    if (!integerIn(screen.width, 1, 32768) || !integerIn(screen.height, 1, 32768)
        || !integerIn(screen.dpi_x, 1, 960) || !integerIn(screen.dpi_y, 1, 960)) return false;
    const step = value.step;
    if (!step?.params) return false;
    const only = (...keys: string[]) => Object.keys(step.params).length === keys.length
        && keys.every((key) => Object.prototype.hasOwnProperty.call(step.params, key));
    switch (step.kind) {
        case 'click':
            return only('x', 'y', 'button') && integerIn(step.params.x, 0, screen.width - 1)
                && integerIn(step.params.y, 0, screen.height - 1)
                && ['primary', 'secondary'].includes(step.params.button);
        case 'key_press':
            return only('key') && ['enter', 'tab', 'escape', 'backspace', 'delete', 'space', 'arrow_up',
                'arrow_down', 'arrow_left', 'arrow_right', 'home', 'end', 'page_up', 'page_down'].includes(step.params.key);
        case 'type_text':
            return only('text') && typeof step.params.text === 'string' && step.params.text.length > 0
                && Array.from(step.params.text).length <= 64 && !/[\u0000-\u001f\u007f-\u009f]/u.test(step.params.text);
        case 'scroll':
            return only('horizontal', 'vertical') && integerIn(step.params.horizontal, -1200, 1200)
                && integerIn(step.params.vertical, -1200, 1200)
                && (step.params.horizontal !== 0 || step.params.vertical !== 0);
        default: return false;
    }
}

export function isOutputInput(item: GrantRequestItemDto): boolean {
    return item.toolName === 'execute_wayland_output_input' || item.providerId === 'desktop.output.input';
}

export function outputApprovalBlocked(item: GrantRequestItemDto): boolean {
    return isOutputInput(item) && (item.toolName !== 'execute_wayland_output_input'
        || item.providerId !== 'desktop.output.input' || item.expectedEffect !== 'input_fallback'
        || item.suggestedMaxUses !== 1 || !validOutputReview(item.waylandOutputConfirmation)
        || item.operationScope.length !== 1 || item.operationScope[0] !== 'wayland_output_input:exact_step'
        || item.resourceScope.length !== 1 || !/^wayland_output:[0-9a-f]{64}$/.test(item.resourceScope[0]));
}

export function OutputConfirmationCard({ value }: { value: WaylandOutputConfirmationDto }) {
    const { t } = useTranslation();
    const step = value.step;
    return <div className="mt-3 space-y-2 rounded-md border p-3 text-xs" data-testid="wayland-output-confirmation">
        <p className="font-semibold">{t('pages.aiAssistant.outputConfirmTitle')}</p>
        <p>{t('pages.aiAssistant.outputConfirmScope')}</p>
        <p>{value.screen.display} · {value.screen.width} × {value.screen.height}</p>
        <p>{t('pages.aiAssistant.outputConfirmOnce')}</p>
        {step.kind === 'click' && <p>{t('pages.aiAssistant.outputConfirmClick', {
            x: step.params.x, y: step.params.y,
            button: t(`pages.aiAssistant.outputButton_${step.params.button}`),
        })}</p>}
        {step.kind === 'key_press' && <p>{t('pages.aiAssistant.outputConfirmKey', { key: step.params.key })}</p>}
        {step.kind === 'scroll' && <p>{t('pages.aiAssistant.outputConfirmScroll', {
            horizontal: step.params.horizontal, vertical: step.params.vertical,
        })}</p>}
        {step.kind === 'type_text' && <>
            <p>{t('pages.aiAssistant.outputConfirmText')}</p>
            <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words rounded bg-muted p-2">{step.params.text}</pre>
        </>}
    </div>;
}
