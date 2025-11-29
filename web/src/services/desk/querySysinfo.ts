// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get system information GET /api/desk/sysinfo */
export async function querySysinfo(options?: { [key: string]: any }) {
  return request<API.RestResponseSystemInfo>('/api/desk/sysinfo', {
    method: 'GET',
    ...(options || {}),
  });
}
