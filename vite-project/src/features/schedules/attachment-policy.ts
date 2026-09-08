import type { ScheduleManagementResponse } from '@/services/types';

type Contract = NonNullable<Extract<ScheduleManagementResponse, { result: 'task_contract' }>['contract']>;
export type AttachmentPolicy = NonNullable<Extract<Contract['permissions'][number]['input'], { kind: 'generated_message' }>['attachment_policy']>;
export const attachmentLimitKeys = ['max_count', 'max_bytes_per_attachment', 'max_total_bytes'] as const;

export function validAttachmentPolicy(policy: AttachmentPolicy | null | undefined): boolean {
    if (policy == null) return true;
    const validLimits = (scope: AttachmentPolicy['automatic']) => {
        if (!scope || !attachmentLimitKeys.every(key => Number.isSafeInteger(scope[key]) && scope[key] >= 0)
            || !Array.isArray(scope.media_types) || new Set(scope.media_types).size !== scope.media_types.length) return false;
        if (scope.max_count === 0) return scope.max_bytes_per_attachment === 0 && scope.max_total_bytes === 0 && scope.media_types.length === 0;
        return scope.max_count <= 32 && scope.max_bytes_per_attachment > 0 && scope.max_bytes_per_attachment <= 64 * 1024 * 1024
            && scope.max_total_bytes >= scope.max_bytes_per_attachment && scope.max_total_bytes <= 128 * 1024 * 1024
            && scope.media_types.length > 0 && scope.media_types.length <= 32 && scope.media_types.every(type => typeof type === 'string'
                && (type === 'text/plain;charset=utf-8' || (type.length <= 128 && /^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+$/.test(type))));
    };
    return validLimits(policy.automatic) && validLimits(policy.approval_ceiling)
        && attachmentLimitKeys.every(key => policy.automatic[key] <= policy.approval_ceiling[key])
        && policy.automatic.media_types.every(type => policy.approval_ceiling.media_types.includes(type));
}
