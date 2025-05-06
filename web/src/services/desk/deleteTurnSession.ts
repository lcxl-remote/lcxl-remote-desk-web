// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Delete turn server session DELETE /api/turn/session */
export async function deleteTurnSession(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.deleteTurnSessionParams,
  options?: { [key: string]: any },
) {
  return request<any>('/api/turn/session', {
    method: 'DELETE',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
