// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Login user account POST /api/login/account */
export async function loginAccount(body: API.LoginParams, options?: { [key: string]: any }) {
  return request<API.LoginResult>('/api/login/account', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
