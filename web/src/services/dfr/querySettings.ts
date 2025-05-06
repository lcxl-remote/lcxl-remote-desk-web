// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Query settings GET /api/dfr/settings */
export async function querySettings(options?: { [key: string]: any }) {
  return request<API.RestResponseSystemSettings>('/api/dfr/settings', {
    method: 'GET',
    ...(options || {}),
  });
}
