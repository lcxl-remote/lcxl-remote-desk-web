import type { TFunction } from 'i18next';

// Display-only translations: never use labels as submitted authorization values.
function label(t: TFunction, group: string, value: string): string {
    const key = `pages.aiAssistant.${group}.${value}`;
    // Protocol values may contain ':' and '.'. Treat the entire flat locale key
    // literally instead of letting i18next parse namespaces or nested paths.
    const translated = t(key, { nsSeparator: false, keySeparator: false });
    return translated === key ? value : translated;
}

export const permissionToolLabel = (t: TFunction, value: string) => label(t, 'permissionTool', value);
export const permissionEffectLabel = (t: TFunction, value: string) => label(t, 'permissionEffect', value);

export function permissionOperationLabel(t: TFunction, value: string): string {
    if (/^(ui|background_input):/.test(value)) {
        const key = `pages.aiAssistant.uiAction_${value.slice(value.indexOf(':') + 1)}`;
        const translated = t(key, { nsSeparator: false, keySeparator: false });
        return translated === key ? value : translated;
    }
    const operation = label(t, 'permissionOperation', value);
    return operation === value ? permissionToolLabel(t, value) : operation;
}

export function permissionResourceLabel(t: TFunction, value: string, applicationName?: string | null): string {
    if (applicationName && value.startsWith('ui_application:')) return applicationName;
    const exact = label(t, 'permissionResource', value);
    if (exact !== value) return exact;
    const separator = value.indexOf(':');
    if (separator < 0) return value;
    const key = `pages.aiAssistant.permissionResourceKind.${value.slice(0, separator)}`;
    // Keep the full identity/path: distinct resources must remain distinguishable.
    const translated = t(key, { value: value.slice(separator + 1), nsSeparator: false, keySeparator: false });
    return translated === key ? value : translated;
}
