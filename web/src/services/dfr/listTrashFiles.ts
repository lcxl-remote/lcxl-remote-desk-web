// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** List trash files GET /api/dfr/trash/list */
export async function listTrashFiles(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.listTrashFilesParams,
  options?: { [key: string]: any },
) {
  return request<API.TrashFileInfoList>('/api/dfr/trash/list', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
