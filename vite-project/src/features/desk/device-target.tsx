import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import { DeviceTargetBoundary } from './device-target-context';

export default function OwnerDeviceTarget() {
    const user = useGetCurrentUser();
    const account = user.data?.data;
    const accountKey = account && account.access !== 'device_user' && account.target_connection_id == null
        ? `u:${account.user_id ?? 'owner'}` : null;
    return <DeviceTargetBoundary accountKey={accountKey} />;
}
