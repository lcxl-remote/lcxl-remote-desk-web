import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';

/**
 * A resolved usage query window. Both bounds are RFC3339 timestamps and `to` is
 * exclusive; an empty object asks the backend for its default recent window.
 * The picker never sends `granularity` — the backend picks hour vs. day from the
 * span width and echoes the effective choice back in `UsageRangeDto`.
 */
export interface UsageRangeParams {
    from?: string;
    to?: string;
}

/** The effective range echoed by the backend, mirrored from `UsageRangeDto`. */
export interface UsageEffectiveRange {
    from: string;
    to: string;
    granularity: string;
}

/** Non-custom presets, each a fixed look-back window ending at "now". */
export type UsagePreset = '24h' | '7d' | '30d';

/**
 * Compute the `[from, to)` bounds for a preset relative to `now`. Kept pure and
 * exported so the preset arithmetic is unit-testable without rendering.
 */
export function presetRange(preset: UsagePreset, now: Date): UsageRangeParams {
    const from = new Date(now.getTime());
    if (preset === '24h') {
        from.setUTCHours(from.getUTCHours() - 24);
    } else if (preset === '7d') {
        from.setUTCDate(from.getUTCDate() - 7);
    } else {
        from.setUTCDate(from.getUTCDate() - 30);
    }
    return { from: from.toISOString(), to: now.toISOString() };
}

/** Format an RFC3339 timestamp for a native `datetime-local` input (local tz). */
function isoToLocalInput(iso: string | undefined): string {
    if (!iso) return '';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '';
    // datetime-local wants `YYYY-MM-DDTHH:mm` in local time; subtract the tz
    // offset so `toISOString().slice` yields local wall-clock rather than UTC.
    const local = new Date(d.getTime() - d.getTimezoneOffset() * 60_000);
    return local.toISOString().slice(0, 16);
}

/** Parse a native `datetime-local` value (local tz) back to an RFC3339 string. */
function localInputToIso(local: string): string | undefined {
    if (!local) return undefined;
    const d = new Date(local);
    return Number.isNaN(d.getTime()) ? undefined : d.toISOString();
}

export interface UsageRangePickerProps {
    value: UsageRangeParams;
    onChange: (next: UsageRangeParams) => void;
    /** Effective range echoed by the backend, rendered as a caption when present. */
    effective?: UsageEffectiveRange;
    disabled?: boolean;
}

const PRESETS: UsagePreset[] = ['24h', '7d', '30d'];

/**
 * Time-range selector shared by every usage page (portable signal + manager
 * console). It offers fixed look-back presets plus a custom `[from, to)` range,
 * and renders the backend's effective window/granularity so a wide range that
 * was folded to UTC days is clearly labelled.
 */
export function UsageRangePicker({ value, onChange, effective, disabled }: UsageRangePickerProps) {
    const { t } = useTranslation();
    const [custom, setCustom] = useState(false);

    const applyPreset = (preset: UsagePreset) => {
        setCustom(false);
        onChange(presetRange(preset, new Date()));
    };

    const setCustomBound = (bound: 'from' | 'to', local: string) => {
        onChange({ ...value, [bound]: localInputToIso(local) });
    };

    return (
        <div className="flex flex-col gap-3">
            <div className="flex flex-wrap items-center gap-2">
                {PRESETS.map((preset) => (
                    <Button
                        key={preset}
                        type="button"
                        size="sm"
                        variant={custom ? 'outline' : 'default'}
                        disabled={disabled}
                        onClick={() => applyPreset(preset)}
                    >
                        {t(`pages.usageRange.preset.${preset}`)}
                    </Button>
                ))}
                <Button
                    type="button"
                    size="sm"
                    variant={custom ? 'default' : 'outline'}
                    disabled={disabled}
                    onClick={() => setCustom(true)}
                >
                    {t('pages.usageRange.preset.custom')}
                </Button>
            </div>

            {custom && (
                <div className="flex flex-wrap items-end gap-3">
                    <label className="flex flex-col gap-1 text-xs text-muted-foreground">
                        {t('pages.usageRange.from')}
                        <Input
                            type="datetime-local"
                            className="w-56"
                            value={isoToLocalInput(value.from)}
                            disabled={disabled}
                            onChange={(e) => setCustomBound('from', e.target.value)}
                        />
                    </label>
                    <label className="flex flex-col gap-1 text-xs text-muted-foreground">
                        {t('pages.usageRange.to')}
                        <Input
                            type="datetime-local"
                            className="w-56"
                            value={isoToLocalInput(value.to)}
                            disabled={disabled}
                            onChange={(e) => setCustomBound('to', e.target.value)}
                        />
                    </label>
                </div>
            )}

            {effective && (
                <p className="text-xs text-muted-foreground">
                    {t('pages.usageRange.effective', {
                        from: new Date(effective.from).toLocaleString(),
                        to: new Date(effective.to).toLocaleString(),
                    })}
                    {effective.granularity === 'day' && ` · ${t('pages.usageRange.dayUtcNote')}`}
                </p>
            )}
        </div>
    );
}
