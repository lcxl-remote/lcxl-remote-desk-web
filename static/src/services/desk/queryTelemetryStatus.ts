// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Query telemetry status GET /api/desk/telemetry/status */
export async function queryTelemetryStatus(options?: { [key: string]: any }) {
  return request<API.RestResponseTelemetryStatus>('/api/desk/telemetry/status', {
    method: 'GET',
    ...(options || {}),
  });
}
