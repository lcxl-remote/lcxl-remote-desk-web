// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** List all online desk sessions GET /api/desk/sessions */
export async function listSessions(options?: { [key: string]: any }) {
  return request<API.SessionModel[]>('/api/desk/sessions', {
    method: 'GET',
    ...(options || {}),
  });
}
