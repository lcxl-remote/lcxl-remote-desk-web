// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Open terminal session GET /api/desk/terminal/${param0} */
export async function openTerminalSession(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.openTerminalSessionParams,
  options?: { [key: string]: any },
) {
  const { session_id: param0, ...queryParams } = params;
  return request<any>(`/api/desk/terminal/${param0}`, {
    method: 'GET',
    params: {
      ...queryParams,
    },
    ...(options || {}),
  });
}
