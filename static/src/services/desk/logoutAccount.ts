// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Logout user account POST /api/login/outLogin */
export async function logoutAccount(options?: { [key: string]: any }) {
  return request<any>('/api/login/outLogin', {
    method: 'POST',
    ...(options || {}),
  });
}
