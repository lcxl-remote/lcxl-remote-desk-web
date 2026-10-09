import type * as Clients from '@/services/clients';
import { deskErrorCodeEnum } from '@/services/types';
import type { MetricsApi } from './dashboard';

type MetricsClients = Pick<typeof Clients, 'getModelMetricsStatus' | 'getModelMetricsOverview' | 'getModelMetricsSeries' | 'getModelMetricsModels' | 'getModelMetricsTools' | 'getModelMetricsRuntime' | 'getModelMetricsCalls' | 'getModelMetricsUnassociated' | 'getModelMetricsCall' | 'getModelMetricsSettings' | 'updateModelMetricsSettings'>;

function data<T>(response: { data?: T | null }): T {
    if (response.data == null) throw new Error('Model metrics unavailable');
    return response.data;
}

export function isMetricsRevisionConflict(error: unknown): boolean {
    return !!error && typeof error === 'object' && 'code' in error && error.code === deskErrorCodeEnum.REVISION_CONFLICT;
}

export function createMetricsApi(clients: MetricsClients): MetricsApi {
    return {
        status: (signal) => clients.getModelMetricsStatus({ signal }).then((response) => data(response)),
        overview: (query, signal) => clients.getModelMetricsOverview(query, { signal }).then((response) => data(response)),
        series: (query, signal) => clients.getModelMetricsSeries(query, { signal }).then((response) => data(response)),
        models: (query, signal) => clients.getModelMetricsModels(query, { signal }).then((response) => data(response)),
        tools: (query, signal) => clients.getModelMetricsTools(query, { signal }).then((response) => data(response)),
        runtime: (query, signal) => clients.getModelMetricsRuntime(query, { signal }).then((response) => data(response)),
        calls: (query, signal) => clients.getModelMetricsCalls(query, { signal }).then((response) => data(response)),
        unassociated: (query, signal) => clients.getModelMetricsUnassociated(query, { signal }).then((response) => data(response)),
        detail: (id, signal) => clients.getModelMetricsCall(id, { signal }).then((response) => data(response)),
        settings: (signal) => clients.getModelMetricsSettings({ signal }).then((response) => data(response)),
        save: (settings) => clients.updateModelMetricsSettings(settings).then((response) => data(response)),
        accessError(error) {
            if (!error || typeof error !== 'object') return false;
            if ('code' in error && error.code === deskErrorCodeEnum.PERMISSION_ERROR) return true;
            if ('response' in error && error.response && typeof error.response === 'object' && 'status' in error.response) {
                return error.response.status === 401 || error.response.status === 403;
            }
            return false;
        },
    };
}
