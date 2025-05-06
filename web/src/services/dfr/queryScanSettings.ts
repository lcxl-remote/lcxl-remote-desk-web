// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get scan settings GET /api/dfr/scan/settings */
export async function queryScanSettings(options?: { [key: string]: any }) {
  return request<API.RestResponseScanSettings>('/api/dfr/scan/settings', {
    method: 'GET',
    ...(options || {}),
  });
}
