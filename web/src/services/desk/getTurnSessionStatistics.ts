// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get turn server session statistics GET /api/turn/session/statistics */
export async function getTurnSessionStatistics(
  // 叠加生成的Param类型 (非body参数swagger默认没有生成对象)
  params: API.getTurnSessionStatisticsParams,
  options?: { [key: string]: any },
) {
  return request<API.TurnSessionStatistics>('/api/turn/session/statistics', {
    method: 'GET',
    params: {
      ...params,
    },
    ...(options || {}),
  });
}
