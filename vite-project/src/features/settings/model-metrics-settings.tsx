import * as clients from '@/services/clients';
import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import { useQueryServerInfo } from '@/services/hooks/systemController/useQueryServerInfo';
import { canViewLocalModelMetrics } from '@/features/model-metrics/access';
import { createMetricsApi } from '@/features/model-metrics/api';
import { MetricsCollectionSettings } from '@/features/model-metrics/settings';

const api = createMetricsApi(clients);
export default function ModelMetricsSettingsPage() {
    const user = useGetCurrentUser().data?.data;
    const mode = useQueryServerInfo().data?.data?.startup_mode;
    const allowed = canViewLocalModelMetrics({ access: user?.access,
        targetConnectionId: user?.target_connection_id, startupMode: mode });
    const identity = `${user?.user_id ?? user?.name ?? ''}:${user?.access ?? ''}:${user?.target_connection_id ?? ''}:${mode ?? ''}`;
    return <MetricsCollectionSettings api={api} allowed={allowed} identity={identity}/>;
}
