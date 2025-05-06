// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Get captcha for login POST /api/login/captcha */
export async function getCaptcha(body: API.FakeCaptchaParams, options?: { [key: string]: any }) {
  return request<API.FakeCaptcha>('/api/login/captcha', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
