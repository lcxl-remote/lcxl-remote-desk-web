import * as clients from '@/services/clients';
import { deskErrorCodeEnum } from '@/services/types';

type ApprovalClients = Pick<typeof clients, 'listApprovalReviews' | 'getApprovalReview' | 'getApprovalReviewParts' | 'getApprovalTraceSettings' | 'updateApprovalTraceSettings' | 'getApprovalMetricsOverview'>;
function data<T>(response: { success?: boolean; code?: number; message?: string | null; data?: T | null }): T {
    if (response.success === false || response.data == null) throw Object.assign(new Error(response.message || 'Approval observation unavailable'), { code: response.code });
    return response.data;
}
export function createApprovalApi(value: ApprovalClients) {
    return {
        list: (query: Parameters<ApprovalClients['listApprovalReviews']>[0], signal?: AbortSignal) => value.listApprovalReviews(query, { signal }).then(data),
        detail: (query: Parameters<ApprovalClients['getApprovalReview']>[0], signal?: AbortSignal) => value.getApprovalReview(query, { signal }).then(data),
        parts: (query: Parameters<ApprovalClients['getApprovalReviewParts']>[0], signal?: AbortSignal) => value.getApprovalReviewParts(query, { signal }).then(data),
        settings: (signal?: AbortSignal) => value.getApprovalTraceSettings({ signal }).then(data),
        save: (settings: Parameters<ApprovalClients['updateApprovalTraceSettings']>[0]) => value.updateApprovalTraceSettings(settings).then(data),
        metrics: (query: Parameters<ApprovalClients['getApprovalMetricsOverview']>[0], signal?: AbortSignal) => value.getApprovalMetricsOverview(query, { signal }).then(data),
    };
}
export type ApprovalApi = ReturnType<typeof createApprovalApi>;
export const approvalApi = createApprovalApi(clients);
export function approvalAccessError(error: unknown) {
    return !!error && typeof error === 'object' && 'code' in error && error.code === deskErrorCodeEnum.PERMISSION_ERROR;
}
export function approvalRevisionConflict(error: unknown) {
    return !!error && typeof error === 'object' && 'code' in error && error.code === deskErrorCodeEnum.REVISION_CONFLICT;
}
