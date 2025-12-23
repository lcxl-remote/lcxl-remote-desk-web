// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Turn server metrics GET /api/turn/metrics */
export async function getTurnMetrics(options?: { [key: string]: any }) {
  return request<string>('/api/turn/metrics', {
    method: 'GET',
    ...(options || {}),
  });
}
