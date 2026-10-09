import { startupModeEnum, type StartupMode } from '@/services/types';

export function canViewLocalModelMetrics({ access, targetConnectionId, startupMode }: {
    access?: string | null;
    targetConnectionId?: string | null;
    startupMode?: StartupMode | null;
}): boolean {
    return access === 'admin' && targetConnectionId == null
        && (startupMode === startupModeEnum.default || startupMode === startupModeEnum.signaling
            || startupMode === startupModeEnum['service-daemon']);
}
