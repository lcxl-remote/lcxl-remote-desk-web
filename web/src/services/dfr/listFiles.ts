// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** List files GET /api/dfr/list */
export async function listFiles(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.listFilesParams,
  options?: { [key: string]: any },
) {
  return request<API.FileInfoList>('/api/dfr/list', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
