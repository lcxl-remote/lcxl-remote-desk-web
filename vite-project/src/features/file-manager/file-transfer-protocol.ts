// Per-chunk DC payload size for uploads (browser → host). Must stay in
// sync with `FILE_TRANSFER_CHUNK_SIZE_TX` in the Rust dispatcher.
// 240 KiB leaves room for the 40-byte protocol header below Chrome's
// typical 256 KiB SCTP message limit.
export const FILE_TRANSFER_CHUNK_SIZE = 240 * 1024;

const TRANSFER_ID_SIZE = 36;
const CHUNK_INDEX_SIZE = 4;
const BINARY_HEADER_SIZE = TRANSFER_ID_SIZE + CHUNK_INDEX_SIZE;

export interface DownloadRequest {
    type: 'download_request';
    transfer_id: string;
    file_path: string;
}

export interface DownloadResponse {
    type: 'download_response';
    transfer_id: string;
    file_name: string;
    file_size: number;
    chunk_size: number;
    total_chunks: number;
}

export interface UploadRequest {
    type: 'upload_request';
    transfer_id: string;
    target_dir: string;
    file_name: string;
    file_size: number;
    chunk_size: number;
    total_chunks: number;
}

export interface UploadResponse {
    type: 'upload_response';
    transfer_id: string;
    accepted: boolean;
    message?: string;
}

export interface TransferComplete {
    type: 'transfer_complete';
    transfer_id: string;
}

export interface TransferError {
    type: 'transfer_error';
    transfer_id: string;
    message: string;
}

export interface TransferCancel {
    type: 'transfer_cancel';
    transfer_id: string;
}

export type FileTransferMessage =
    | DownloadRequest
    | DownloadResponse
    | UploadRequest
    | UploadResponse
    | TransferComplete
    | TransferError
    | TransferCancel;

export interface BinaryChunk {
    transferId: string;
    chunkIndex: number;
    chunkData: Uint8Array;
}

export function parseBinaryChunk(data: ArrayBuffer): BinaryChunk | null {
    if (data.byteLength < BINARY_HEADER_SIZE) return null;
    const view = new DataView(data);
    const decoder = new TextDecoder('utf-8');
    const transferId = decoder.decode(new Uint8Array(data, 0, TRANSFER_ID_SIZE));
    const chunkIndex = view.getUint32(TRANSFER_ID_SIZE, false);
    const chunkData = new Uint8Array(data, BINARY_HEADER_SIZE);
    return { transferId, chunkIndex, chunkData };
}

export function buildBinaryChunk(
    transferId: string,
    chunkIndex: number,
    data: Uint8Array,
): ArrayBuffer {
    const idBytes = new TextEncoder().encode(transferId);
    const buffer = new ArrayBuffer(BINARY_HEADER_SIZE + data.length);
    const view = new DataView(buffer);
    new Uint8Array(buffer).set(idBytes, 0);
    view.setUint32(TRANSFER_ID_SIZE, chunkIndex, false);
    new Uint8Array(buffer, BINARY_HEADER_SIZE).set(data);
    return buffer;
}
