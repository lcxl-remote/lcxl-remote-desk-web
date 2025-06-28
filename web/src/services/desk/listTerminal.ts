// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** List terminal GET /api/desk/terminals */
export async function listTerminal(options?: { [key: string]: any }) {
  return request<API.TerminalList>('/api/desk/terminals', {
    method: 'GET',
    ...(options || {}),
  });
}
