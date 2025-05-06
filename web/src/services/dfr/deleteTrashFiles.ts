// @ts-ignore
/* eslint-disable */
import { request } from '@umijs/max';

/** Delete trash files DELETE /api/dfr/trash/files */
export async function deleteTrashFiles(
  body: API.DeleteTrashFilesRequest,
  options?: { [key: string]: any },
) {
  return request<any>('/api/dfr/trash/files', {
    method: 'DELETE',
    headers: {
      'Content-Type': 'application/json',
    },
    data: body,
    ...(options || {}),
  });
}
