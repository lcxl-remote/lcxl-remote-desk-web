import { useTranslation } from 'react-i18next';

function record(value: unknown): Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
        ? value as Record<string, unknown> : {};
}

export function AssistantObservationResult({ data }: { data: unknown }) {
    const { t } = useTranslation();
    const label = (name: string, options?: Record<string, unknown>) => t(`pages.deviceAssistant.observation.${name}`, options);
    const context = record(record(data).ReadContext);
    const session = record(context.DesktopSessionInspect);
    const ui = record(context.DesktopUiInspect);
    const nodes = Array.isArray(ui.nodes) ? ui.nodes.map(record) : null;
    const roles: Record<string, string> = {
        application: 'application', window: 'window', button: 'button', textbox: 'text',
        textfield: 'text', textarea: 'text', text: 'text', statictext: 'text',
        checkbox: 'checkbox', radiobutton: 'radio', combobox: 'combo',
        scrollarea: 'scrollArea', scrollbar: 'scrollBar', menu: 'menu', menuitem: 'menuItem',
        group: 'group', list: 'list', row: 'row', table: 'table', tab: 'tab', slider: 'slider',
    };
    const knownActions = ['invoke', 'select', 'focus', 'toggle', 'set_value'];
    return <div className="space-y-3 text-sm">
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
        </> : <p>{label('unrecognized')}</p>}
        <details className="rounded-md border p-3">
            <summary className="cursor-pointer text-muted-foreground">{t('pages.deviceAssistant.workspace.technicalDetails')}</summary>
            <pre data-testid="observation-output" className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap break-words text-xs">{JSON.stringify(data, null, 2)}</pre>
        </details>
    </div>;
}
