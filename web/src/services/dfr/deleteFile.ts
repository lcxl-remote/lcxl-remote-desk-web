// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Delete a file DELETE /api/dfr/file */
export async function deleteFile(body: API.DeleteFileRequest, options?: { [key: string]: any }) {
  return request<any>('/api/dfr/file', {
    method: 'DELETE',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
