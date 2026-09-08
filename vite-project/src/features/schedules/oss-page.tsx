import { assistantPaths, assistantConnections } from './assistant-paths';
import { useListConnections } from '@/services/hooks/connectionController/useListConnections';
import SchedulePage from './page';
export default function OssSchedulesPage() {
    const query = useListConnections({ query: { refetchInterval: 10_000 } });
    const online = query.isError ? [] : (query.data ?? []);
    const paths = assistantPaths(online);
    const connectionIds = assistantConnections(online);
    const devices = (query.data ?? []).flatMap(connection => {
        const id = connection.version_info.client_id;
        return id ? [{ id, name: id, connectionId: connectionIds[id], assistantPath: paths[id] }] : [];
    });
    return <SchedulePage devices={devices} loadingDevices={query.isLoading} />;
}
