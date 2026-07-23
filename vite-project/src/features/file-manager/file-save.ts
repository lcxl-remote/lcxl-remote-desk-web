import {
    type BlobSaver,
    type WritableFileStreamLike,
} from './download-sink';

async function saveFileWithPicker(blob: Blob, fileName: string): Promise<boolean> {
    if (!('showSaveFilePicker' in window)) return false;
    try {
        const handle = await (window as any).showSaveFilePicker({
            suggestedName: fileName,
        });
        const writable = await handle.createWritable();
        await writable.write(blob);
        await writable.close();
        return true;
    } catch {
        return false;
    }
}

function triggerBrowserDownload(blob: Blob, fileName: string) {
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = fileName;
    document.body.appendChild(anchor);
    anchor.click();
    document.body.removeChild(anchor);
    URL.revokeObjectURL(url);
}

export function canStreamToDisk(): boolean {
    return typeof window !== 'undefined' && 'showSaveFilePicker' in window;
}

export async function openStreamingWritable(
    fileName: string,
): Promise<WritableFileStreamLike | null> {
    if (!canStreamToDisk()) return null;
    try {
        const handle = await (window as any).showSaveFilePicker({ suggestedName: fileName });
        return (await handle.createWritable()) as WritableFileStreamLike;
    } catch {
        return null;
    }
}

export const fallbackBlobSaver: BlobSaver = async (blob, fileName) => {
    const saved = await saveFileWithPicker(blob, fileName);
    if (!saved) triggerBrowserDownload(blob, fileName);
};
