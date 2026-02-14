// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** List terminal GET /api/desk/terminals/${param0} */
export async function listTerminal(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.listTerminalParams,
  options?: { [key: string]: any },
) {
  const { session_id: param0, ...queryParams } = params;
  return request<API.TerminalList>(`/api/desk/terminals/${param0}`, {
    method: 'GET',
    params: { ...queryParams },
    ...(options || {}),
  });
}
