// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Change password of user account POST /api/desk/api/login/password */
export async function changePassword(body: API.PasswordParams, options?: { [key: string]: any }) {
  return request<any>('/api/desk/api/login/password', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
