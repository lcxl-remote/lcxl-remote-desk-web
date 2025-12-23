// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get turn server info GET /api/turn/info */
export async function getTurnInfo(options?: { [key: string]: any }) {
  return request<API.TurnInfo>('/api/turn/info', {
    method: 'GET',
    ...(options || {}),
  });
}
