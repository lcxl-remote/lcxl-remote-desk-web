// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get current user GET /api/currentUser */
export async function getCurrentUser(options?: { [key: string]: any }) {
  return request<API.UserResponeCurrentUser>('/api/currentUser', {
    method: 'GET',
    ...(options || {}),
  });
}
