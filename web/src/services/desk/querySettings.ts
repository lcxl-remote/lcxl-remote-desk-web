// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Query settings GET /api/desk/settings */
export async function querySettings(options?: { [key: string]: any }) {
  return request<API.RestResponseSystemSettings>('/api/desk/settings', {
    method: 'GET',
    ...(options || {}),
  });
}
