// Reusable editor for a per-code capability ceiling (a `SecuritySettings` value).
//
// Shared by the open-source device-code list and the manager device page (rule 22:
// one control, identical shape on both). Form-agnostic — it takes the current value
// and an onChange, so it composes with either local state or a form field.
//
// Each dimension is a three-state select: allow / ask (prompt) / deny, matching the
// host-side security page. The ceiling is the *maximum* a redeemer of this code may
// reach; the host still applies `meet(ceiling, global)` plus live approval, so
// "allow" here only means "do not further restrict", never "bypass the owner".

import { useTranslation } from 'react-i18next';

import { Button } from '@/components/ui/button';
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from '@/components/ui/select';
import type { SecuritySettings } from '@/services/types';

// The dimensions an access grant can scope, with the shared i18n label/description
// keys reused from the host security page.
const DIMENSIONS: { key: CapabilityDimension; label: string; desc: string }[] = [
    { key: 'allow_remote_control', label: 'security.permission.remoteControl', desc: 'pages.system.security.remoteControlDesc' },
    { key: 'allow_clipboard_sync', label: 'security.permission.clipboardSync', desc: 'pages.system.security.clipboardSyncDesc' },
    { key: 'allow_system_audio_capture', label: 'security.permission.systemAudioCapture', desc: 'pages.system.security.systemAudioCaptureDesc' },
    { key: 'allow_private_screen', label: 'security.permission.privateScreen', desc: 'pages.system.security.privateScreenDesc' },
    { key: 'allow_whiteboard', label: 'security.permission.whiteboard', desc: 'pages.system.security.whiteboardDesc' },
    { key: 'allow_terminal', label: 'security.permission.terminal', desc: 'pages.system.security.terminalDesc' },
    { key: 'allow_file_browse', label: 'security.permission.fileBrowse', desc: 'pages.system.security.fileBrowseDesc' },
    { key: 'allow_file_delete', label: 'security.permission.fileDelete', desc: 'pages.system.security.fileDeleteDesc' },
    { key: 'allow_file_transfer', label: 'security.permission.fileTransfer', desc: 'pages.system.security.fileTransferDesc' },
];

type CapabilityDimension =
    | 'allow_remote_control'
    | 'allow_clipboard_sync'
    | 'allow_system_audio_capture'
    | 'allow_private_screen'
    | 'allow_whiteboard'
    | 'allow_terminal'
    | 'allow_file_browse'
    | 'allow_file_delete'
    | 'allow_file_transfer';

export const toSelect = (v: boolean | null | undefined): string =>
    v === true ? 'allow' : v === false ? 'deny' : 'prompt';

export const fromSelect = (v: string): boolean | null =>
    v === 'allow' ? true : v === 'deny' ? false : null;

// Preset ceilings. "View only" denies every grantable action (watch the screen but
// touch nothing); "Assist" grants the common support capabilities and leaves the
// riskier ones to prompt; "Full" allows everything (still subject to host meet).
export const CAPABILITY_PRESETS: Record<'viewOnly' | 'assist' | 'full', SecuritySettings> = {
    viewOnly: {
        allow_remote_control: false,
        allow_clipboard_sync: false,
        allow_system_audio_capture: false,
        allow_private_screen: false,
        allow_whiteboard: false,
        allow_terminal: false,
        allow_file_browse: false,
        allow_file_delete: false,
        allow_file_transfer: false,
    },
    assist: {
        allow_remote_control: true,
        allow_clipboard_sync: true,
        allow_system_audio_capture: true,
        allow_private_screen: null,
        allow_whiteboard: true,
        allow_terminal: false,
        allow_file_browse: true,
        allow_file_delete: null,
        allow_file_transfer: true,
    },
    full: {
        allow_remote_control: true,
        allow_clipboard_sync: true,
        allow_system_audio_capture: true,
        allow_private_screen: true,
        allow_whiteboard: true,
        allow_terminal: true,
        allow_file_browse: true,
        allow_file_delete: true,
        allow_file_transfer: true,
    },
};

export interface CapabilityCeilingEditorProps {
    // The current ceiling; `null` is treated as all-ask (no explicit config).
    value: SecuritySettings | null;
    onChange: (value: SecuritySettings) => void;
}

export function CapabilityCeilingEditor({ value, onChange }: CapabilityCeilingEditorProps) {
    const { t } = useTranslation();
    const ceiling: SecuritySettings = value ?? {};

    const setDimension = (key: CapabilityDimension, select: string) => {
        onChange({ ...ceiling, [key]: fromSelect(select) });
    };

    return (
        <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm text-muted-foreground">{t('pages.capabilityCeiling.presets')}</span>
                <Button type="button" size="sm" variant="outline" onClick={() => onChange({ ...CAPABILITY_PRESETS.viewOnly })}>
                    {t('pages.capabilityCeiling.preset.viewOnly')}
                </Button>
                <Button type="button" size="sm" variant="outline" onClick={() => onChange({ ...CAPABILITY_PRESETS.assist })}>
                    {t('pages.capabilityCeiling.preset.assist')}
                </Button>
                <Button type="button" size="sm" variant="outline" onClick={() => onChange({ ...CAPABILITY_PRESETS.full })}>
                    {t('pages.capabilityCeiling.preset.full')}
                </Button>
            </div>
            <p className="text-xs text-muted-foreground">{t('pages.capabilityCeiling.hint')}</p>
            <div className="space-y-2">
                {DIMENSIONS.map((dim) => (
                    <div key={dim.key} className="flex items-center justify-between gap-4 rounded-lg border p-3">
                        <div className="space-y-0.5">
                            <div className="text-sm font-medium">{t(dim.label)}</div>
                            <div className="text-xs text-muted-foreground">{t(dim.desc)}</div>
                        </div>
                        <div className="w-40 shrink-0">
                            <Select value={toSelect(ceiling[dim.key])} onValueChange={(v) => setDimension(dim.key, v)}>
                                <SelectTrigger>
                                    <SelectValue />
                                </SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="prompt">{t('security.select.prompt')}</SelectItem>
                                    <SelectItem value="allow">{t('security.select.allow')}</SelectItem>
                                    <SelectItem value="deny">{t('security.select.deny')}</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                    </div>
                ))}
            </div>
        </div>
    );
}
