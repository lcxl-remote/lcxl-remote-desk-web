// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Update telemetry consent POST /api/desk/telemetry/consent */
export async function updateTelemetryConsent(
  body: API.TelemetryConsent,
  options?: { [key: string]: any },
) {
  return request<any>('/api/desk/telemetry/consent', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
