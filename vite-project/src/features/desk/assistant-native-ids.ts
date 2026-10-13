/** Encode the complete native identity; this does not create an alias. */
export function assistantMessageId(bytes = crypto.getRandomValues(new Uint8Array(16))): string {
    if (bytes.length !== 16) throw new Error('A native message identity requires 16 bytes');
    return `m_${btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')}`;
}
