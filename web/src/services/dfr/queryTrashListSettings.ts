// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Query list trash file settings GET /api/dfr/trash/list/settings */
export async function queryTrashListSettings(options?: { [key: string]: any }) {
  return request<API.RestResponseTrashListSettings>('/api/dfr/trash/list/settings', {
    method: 'GET',
    ...(options || {}),
  });
}
