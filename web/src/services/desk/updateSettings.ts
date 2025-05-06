// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Update settings POST /api/desk/settings */
export async function updateSettings(body: API.SystemSettings, options?: { [key: string]: any }) {
  return request<any>('/api/desk/settings', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
