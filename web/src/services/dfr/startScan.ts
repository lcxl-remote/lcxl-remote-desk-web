// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Start a new file scan POST /api/dfr/scan/start */
export async function startScan(body: API.ScanSettings, options?: { [key: string]: any }) {
  return request<API.RestResponseI64>('/api/dfr/scan/start', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
