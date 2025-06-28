// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Open terminal session GET /api/desk/terminal */
export async function openTerminalSession(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.openTerminalSessionParams,
  options?: { [key: string]: any },
) {
  return request<API.SignalingModel>('/api/desk/terminal', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
