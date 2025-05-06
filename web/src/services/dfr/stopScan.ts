// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Stop the current file scan POST /api/dfr/scan/stop */
export async function stopScan(options?: { [key: string]: any }) {
  return request<any>('/api/dfr/scan/stop', {
    method: 'POST',
    ...(options || {}),
  });
}
