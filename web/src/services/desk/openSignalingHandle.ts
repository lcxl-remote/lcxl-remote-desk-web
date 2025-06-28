// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Open Signaling Handle, return websocket stream. NOTE: The OpenAPI generated typescript service is not right. GET /api/desk/signaling */
export async function openSignalingHandle(options?: { [key: string]: any }) {
  return request<API.SignalingModel>('/api/desk/signaling', {
    method: 'GET',
    ...(options || {}),
  });
}
