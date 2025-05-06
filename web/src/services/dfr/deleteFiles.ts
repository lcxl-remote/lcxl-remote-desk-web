// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Delete files DELETE /api/dfr/files */
export async function deleteFiles(body: API.DeleteFilesRequest, options?: { [key: string]: any }) {
  return request<any>('/api/dfr/files', {
    method: 'DELETE',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
