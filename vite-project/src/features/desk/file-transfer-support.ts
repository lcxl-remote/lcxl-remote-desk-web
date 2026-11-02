/** Current host implementations, independent of client version or control permission. */
export function supportsFileTransfer(platform: unknown): boolean {
    return platform === 'Windows' || platform === 'Linux' || platform === 'Mac';
}
