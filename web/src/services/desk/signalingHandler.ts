// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Signaling Handler GET /api/desk/signaling */
export async function signalingHandler(options?: { [key: string]: any }) {
  return request<any>('/api/desk/signaling', {
    method: 'GET',
    ...(options || {}),
  });
}
