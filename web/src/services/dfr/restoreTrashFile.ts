// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Restore a trash file POST /api/dfr/trash/file/restore */
export async function restoreTrashFile(
  body: API.DeleteTrashFileRequest,
  options?: { [key: string]: any },
) {
  return request<any>('/api/dfr/trash/file/restore', {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
