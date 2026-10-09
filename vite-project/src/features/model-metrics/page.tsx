import * as clients from '@/services/clients';
import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import { useQueryServerInfo } from '@/services/hooks/systemController/useQueryServerInfo';
import { canViewLocalModelMetrics } from './access';
import { LocalModelUsage } from './local-usage';
import { MetricsDashboard } from './dashboard';
import { createMetricsApi } from './api';

const api = createMetricsApi(clients);
export default function ModelMetricsPage() {
    const user = useGetCurrentUser().data?.data;
    const info = useQueryServerInfo().data?.data;
    const mode = info?.startup_mode;
    const allowed = canViewLocalModelMetrics({ access: user?.access,
        targetConnectionId: user?.target_connection_id, startupMode: mode });
    const identity = `${user?.user_id ?? user?.name ?? ''}:${user?.access ?? ''}:${user?.target_connection_id ?? ''}:${mode ?? ''}`;
    return <MetricsDashboard api={api} allowed={allowed} identity={identity} settingsPath="/system/model-metrics-settings" usage={<LocalModelUsage identity={identity}/>}/>;
}
