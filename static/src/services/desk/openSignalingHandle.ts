// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Open Signaling Handle, return websocket stream. NOTE: The OpenAPI generated typescript service is not right. GET /api/desk/signaling */
export async function openSignalingHandle(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.openSignalingHandleParams,
  options?: { [key: string]: any },
) {
  return request<API.SignalingModel>('/api/desk/signaling', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
