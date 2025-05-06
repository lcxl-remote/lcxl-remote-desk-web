// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Restore trash files POST /api/dfr/trash/files/restore */
export async function restoreTrashFiles(
  body: API.RestoreTrashFilesRequest,
  options?: { [key: string]: any },
) {
  return request<any>('/api/dfr/trash/files/restore', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
