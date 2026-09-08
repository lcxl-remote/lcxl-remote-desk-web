import { getScheduleBudgetPolicy, updateScheduleBudgetPolicy } from '@/services/clients';
import type { TaskBudget } from '@/services/types';
import { BudgetPolicySettings } from '@/features/schedules/budget-policy-settings';

async function load() {
    const result = await getScheduleBudgetPolicy();
    if (!result.success || !result.data) throw new Error('Schedule budget policy unavailable');
    return result.data;
}
async function update(revision: number, maximum: TaskBudget) {
    const result = await updateScheduleBudgetPolicy({ expected_revision: revision, maximum });
    if (!result.success || !result.data) throw new Error('Schedule budget policy could not be saved');
    return result.data;
}
export default function ScheduleBudgetSettings() {
    return <BudgetPolicySettings load={load} update={update} />;
}
