// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Query list file settings GET /api/dfr/list/settings */
export async function queryListSettings(options?: { [key: string]: any }) {
  return request<API.RestResponseListSettings>('/api/dfr/list/settings', {
    method: 'GET',
    ...(options || {}),
  });
}
