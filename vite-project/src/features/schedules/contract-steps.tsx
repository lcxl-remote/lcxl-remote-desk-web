import { useState, type Dispatch, type SetStateAction } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import type { ScheduleManagementResponse } from '@/services/types';

type Contract = NonNullable<Extract<ScheduleManagementResponse, { result: 'task_contract' }>['contract']>;
export function ContractSteps({ draft, setDraft }: { draft: Contract; setDraft: Dispatch<SetStateAction<Contract>> }) {
    const { t } = useTranslation();
    const [removing, setRemoving] = useState<string | null>(null);
    const affected = new Set<string>();
    if (removing && draft.steps.some(step => step.step_id === removing)) {
        affected.add(removing);
        // Stored order is topological: one pass includes every downstream dependency.
        for (const step of draft.steps) if (step.depends_on.some(id => affected.has(id))) affected.add(step.step_id);
    }
    const move = (index: number, offset: number) => {
        const steps = [...draft.steps];
        const next = index + offset;
        if (next < 0 || next >= steps.length) return;
        [steps[index], steps[next]] = [steps[next], steps[index]];
        const seen = new Set<string>();
        for (const step of steps) {
            if (step.depends_on.some(id => !seen.has(id))) return;
            seen.add(step.step_id);
        }
        setDraft(current => ({ ...current, steps }));
    };
    return <section className="space-y-3">
        <h3 className="font-semibold">{t('schedules.editor.steps')}</h3>
        <p className="text-sm">{t('schedules.editor.stepsNote')}</p>
        {draft.steps.map((step, index) => <article key={step.step_id} className="space-y-2 rounded border p-3">
            <p>{index + 1}. {step.step_id} · {draft.permissions.find(rule => rule.rule_id === step.rule_id)?.tool_name}</p>
            <p>{t('schedules.editor.dependencies')}</p>
            {draft.steps.slice(0, index).map(previous => <label className="flex items-center gap-2" key={previous.step_id}>
                <input type="checkbox" checked={step.depends_on.includes(previous.step_id)} onChange={event => {
                    const checked = event.target.checked;
                    setRemoving(null);
                    setDraft(current => ({ ...current, steps: current.steps.map(item => item.step_id !== step.step_id ? item : {
                        ...item, depends_on: checked ? [...item.depends_on, previous.step_id] : item.depends_on.filter(id => id !== previous.step_id),
                    }) }));
                }} />{previous.step_id}
            </label>)}
            <div className="flex flex-wrap gap-2">
                <Button type="button" variant="outline" disabled={index === 0 || step.depends_on.includes(draft.steps[index - 1]?.step_id)} onClick={() => move(index, -1)}>{t('schedules.editor.moveUp')}</Button>
                <Button type="button" variant="outline" disabled={index === draft.steps.length - 1 || draft.steps[index + 1]?.depends_on.includes(step.step_id)} onClick={() => move(index, 1)}>{t('schedules.editor.moveDown')}</Button>
                <Button type="button" variant="outline" onClick={() => setRemoving(step.step_id)}>{t('schedules.editor.removeStep')}</Button>
            </div>
        </article>)}
        {affected.size > 0 && <div className="space-y-2 rounded border p-3">
            <p>{t('schedules.editor.removeStepImpact')}</p>
            <ul className="list-inside list-disc">{draft.steps.filter(step => affected.has(step.step_id)).map(step => <li key={step.step_id}>{step.step_id}</li>)}</ul>
            <div className="flex gap-2">
                <Button type="button" onClick={() => {
                    const rules = new Set(draft.steps.filter(step => affected.has(step.step_id)).map(step => step.rule_id));
                    setDraft(current => ({ ...current, steps: current.steps.filter(step => !affected.has(step.step_id)),
                        permissions: current.permissions.filter(rule => !rules.has(rule.rule_id)) }));
                    setRemoving(null);
                }}>{t('schedules.editor.confirmRemoveSteps')}</Button>
                <Button type="button" variant="outline" onClick={() => setRemoving(null)}>{t('schedules.editor.keepSteps')}</Button>
            </div>
        </div>}
    </section>;
}
