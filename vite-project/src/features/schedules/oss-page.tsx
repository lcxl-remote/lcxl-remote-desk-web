import { scheduleDevices } from './assistant-paths';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import SchedulePage from './page';
export default function OssSchedulesPage() {
    const query = useListConnections({ query: { refetchInterval: 10_000 } });
    const online = query.isError ? [] : (query.data?.data ?? []);
    const devices = scheduleDevices(query.data?.data ?? [], online);
    return <SchedulePage devices={devices} loadingDevices={query.isLoading} />;
}
