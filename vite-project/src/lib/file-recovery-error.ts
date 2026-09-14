import { RestResponseError } from './kubb-client';

const reasons = new Set(['unauthorized', 'invalid_request', 'identity_changed', 'busy', 'storage_unavailable', 'worker_unavailable', 'unsupported', 'material_unavailable', 'material_expired', 'material_cleaning', 'material_cleaned', 'clock_changed']);
export class RecoveryFailure extends Error {
    readonly reason: string;
    constructor(reason: string) { super(reason); this.reason = reason; }
}
export function recoveryErrorKey(error: unknown): string {
    const reason = error instanceof RecoveryFailure ? error.reason
        : error instanceof RestResponseError && typeof error.data === 'string' ? error.data : undefined;
    return reason && reasons.has(reason) ? `error.${reason}` : 'failed';
}
export async function requireRecoveryZip(data: unknown): Promise<Blob> {
    if (data instanceof Blob && data.type.includes('json') && data.size <= 65536) {
        const failure: unknown = JSON.parse(await data.text());
        if (failure && typeof failure === 'object' && 'success' in failure && failure.success === false
            && 'data' in failure && typeof failure.data === 'string' && reasons.has(failure.data)) {
            throw new RecoveryFailure(failure.data);
        }
    }
    if (!(data instanceof Blob) || !data.type.includes('zip')) throw new Error('Recovery export unavailable');
    return data;
}
