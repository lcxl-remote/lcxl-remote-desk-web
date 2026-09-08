import { useTranslation } from 'react-i18next';
import { attachmentLimitKeys, type AttachmentPolicy } from './attachment-policy';

export function ContractAttachments({ policy, original, onChange }: {
    policy: AttachmentPolicy | null | undefined; original?: AttachmentPolicy | null; onChange?: (policy: AttachmentPolicy) => void;
}) {
    const { t } = useTranslation();
    if (!policy) return <p>{t('schedules.attachments.forbidden')}</p>;
    return <section className="space-y-2 rounded border p-3">
        <h4 className="font-semibold">{t('schedules.attachments.title')}</h4>
        {(['automatic', 'approval_ceiling'] as const).map(key => <div className="space-y-2" key={key}>
            <p>{t(`schedules.attachments.${key}`)}</p>
            {attachmentLimitKeys.map(field => <label className="block" key={field}>
                {t(`schedules.attachments.${field}`)}: {onChange ? <input type="number" min={0} step={1} required className="rounded border bg-background p-2 ml-2" value={policy[key][field]}
                    onChange={event => onChange({ ...policy, [key]: { ...policy[key], [field]: Number(event.target.value) } })} /> : policy[key][field]}
            </label>)}
            <p>{t('schedules.attachments.mediaTypes')}</p>
            {(onChange && original ? original[key].media_types : policy[key].media_types).map(type => <label className="block break-all" key={type}>
                {onChange && <input type="checkbox" checked={policy[key].media_types.includes(type)} onChange={event => onChange({ ...policy, [key]: { ...policy[key],
                    media_types: event.target.checked ? [...new Set([...policy[key].media_types, type])] : policy[key].media_types.filter(value => value !== type),
                } })} />} {type}
            </label>)}
        </div>)}
    </section>;
}
