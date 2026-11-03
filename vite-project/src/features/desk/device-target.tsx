import { useGetCurrentUser } from '@/services/hooks/userController/useGetCurrentUser';
import { DeviceTargetBoundary } from './device-target-context';

export default function OwnerDeviceTarget() {
    const user = useGetCurrentUser();
    return <DeviceTargetBoundary accountKey={user.data?.data?.user_id == null ? null : `u:${user.data.data.user_id}`} />;
}
