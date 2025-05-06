// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get notices GET /api/notices */
export async function getNotices(options?: { [key: string]: any }) {
  return request<API.NoticeIconList>('/api/notices', {
    method: 'GET',
    ...(options || {}),
  });
}
