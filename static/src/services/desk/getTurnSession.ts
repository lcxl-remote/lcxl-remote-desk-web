// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get turn server session GET /api/turn/session */
export async function getTurnSession(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.getTurnSessionParams,
  options?: { [key: string]: any },
) {
  return request<API.TurnSession>('/api/turn/session', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
