// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Delete a trash file DELETE /api/dfr/trash/file */
export async function deleteTrashFile(
  body: API.DeleteTrashFileRequest,
  options?: { [key: string]: any },
) {
  return request<any>('/api/dfr/trash/file', {
    method: 'DELETE',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
