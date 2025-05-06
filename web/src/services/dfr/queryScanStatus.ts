// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get scan status GET /api/dfr/scan/status */
export async function queryScanStatus(options?: { [key: string]: any }) {
  return request<API.RestResponseScanStatus>('/api/dfr/scan/status', {
    method: 'GET',
    ...(options || {}),
  });
}
