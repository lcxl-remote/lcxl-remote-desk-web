/** Stable identity for navigation; connection identity only for fixed grants. */
export function devicePath(deviceId: string, page = '', orgId?: number) {
    return `/desk/device/${encodeURIComponent(deviceId)}${page ? `/${page}` : ''}${orgId == null ? '' : `?org_id=${orgId}`}`;
}
export function sessionPath(connectionId: string) {
    return `/desk/session/${encodeURIComponent(connectionId)}`;
}
