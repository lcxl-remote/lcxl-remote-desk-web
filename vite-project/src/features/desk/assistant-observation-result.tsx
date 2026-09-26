import { AssistantCodeBlock } from './assistant-code-block';
import { Disclosure } from '@/components/ui/disclosure';
import { useTranslation } from 'react-i18next';

function record(value: unknown): Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
        ? value as Record<string, unknown> : {};
}

export function AssistantObservationResult({ data }: { data: unknown }) {
    const { t } = useTranslation();
    const label = (name: string, options?: Record<string, unknown>) => t(`pages.aiAssistant.observation.${name}`, options);
    const context = record(record(data).ReadContext);
    const session = record(context.DesktopSessionInspect);
    const screenshot = record(context.ScreenCaptureCurrent);
    const frame = record(screenshot.frame_observation);
    const ui = record(context.DesktopUiInspect);
    const nodes = Array.isArray(ui.nodes) ? ui.nodes.map(record) : null;
    const roles: Record<string, string> = {
        application: 'application', window: 'window', button: 'button', textbox: 'text',
        textfield: 'text', textarea: 'text', text: 'text', statictext: 'text',
        checkbox: 'checkbox', radiobutton: 'radio', combobox: 'combo',
        scrollarea: 'scrollArea', scrollbar: 'scrollBar', menu: 'menu', menuitem: 'menuItem',
        group: 'group', list: 'list', row: 'row', table: 'table', tab: 'tab', slider: 'slider',
    };
    const knownActions = ['invoke', 'select', 'focus', 'toggle', 'set_value', 'scroll'];
    return <div className="space-y-3 text-sm">
        {context.ScreenCaptureCurrent != null && <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 rounded-md bg-muted p-3">
            <dt>{label('frameFreshness')}</dt><dd>{frame.freshness === 'latest_observed' ? label('latestObserved') : frame.freshness === 'fresh' ? label('freshFrame') : frame.freshness === 'unchanged_verified' ? label('unchangedFrame') : label('unavailable')}</dd>
            {typeof frame.receipt_age_ms === 'number' && <><dt>{label('frameAge')}</dt><dd>{label('frameAgeValue', { age: frame.receipt_age_ms })}</dd></>}
            {frame.source_timestamp_ns == null && <><dt>{label('sourceTime')}</dt><dd>{label('unavailable')}</dd></>}
        </dl>}
        {context.DesktopSessionInspect != null ? <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 rounded-md bg-muted p-3">
            <dt>{label('status')}</dt><dd>{label('accessible')}</dd>
            <dt>{label('os')}</dt><dd>{typeof session.os === 'string' ? ({ macos: 'macOS', windows: 'Windows', linux: 'Linux' }[session.os] ?? session.os) : label('unavailable')}</dd>
            <dt>{label('foreground')}</dt><dd className="break-words">{typeof session.active_application_name === 'string' && session.active_application_name.trim() ? session.active_application_name : label('unavailable')}</dd>
        </dl> : nodes ? <>
            <p>{label('count', { count: nodes.length })}</p>
            {ui.truncated === true && <p className="text-muted-foreground">{label('truncated')}</p>}
            {nodes.length === 0 && <p className="text-muted-foreground">{label('empty')}</p>}
            <ul className="max-h-96 space-y-2 overflow-auto" aria-label={label('controls')}>
                {nodes.map((node, index) => {
                    const role = typeof node.role === 'string' ? node.role.toLowerCase().replace(/^ax/, '').replace(/[_\s-]/g, '') : '';
                    const actions = Array.isArray(node.supported_actions) ? node.supported_actions.filter((action): action is string => typeof action === 'string' && knownActions.includes(action)) : [];
                    return <li key={index} className="space-y-1 rounded-md border p-3">
                        <p className="break-words font-medium">{typeof node.name === 'string' && node.name.trim() ? node.name : label('unnamed')}
                            <span className="ml-2 text-xs font-normal text-muted-foreground">{label(`role.${roles[role] ?? 'control'}`)}</span>
                        </p>
                        {node.is_protected === true ? <p>{label('protected')}</p> : typeof node.value === 'string' && node.value !== '' && <p className="whitespace-pre-wrap break-words">{label('value', { value: node.value })}</p>}
                        {node.enabled === false && <p className="text-muted-foreground">{label('disabled')}</p>}
                        {typeof node.application_state === 'string' && ['foreground', 'background', 'hidden'].includes(node.application_state) && <p>{label(node.application_state === 'foreground' ? 'stateForeground' : node.application_state)}</p>}
                        {actions.length > 0 && <p className="text-xs text-muted-foreground">{label('actions', { value: actions.map(action => label(`action.${action}`)).join(' / ') })}</p>}
                    </li>;
                })}
            </ul>
        </> : context.ScreenCaptureCurrent == null ? <p>{label('unrecognized')}</p> : null}
        <Disclosure className="rounded-md border p-3" title={<>{t('pages.aiAssistant.workspace.technicalDetails')}</>} summaryClassName="cursor-pointer text-muted-foreground">

            <AssistantCodeBlock testId="observation-output" text={JSON.stringify(data)} />
        </Disclosure>
    </div>;
}
