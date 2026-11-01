import type { AssistantSourceResolution } from '@/services/types';
import { assistantSourceAnchor } from './assistant-data-api';

/** Offsets are native UTF-8 bytes, never guessed from repeated display text. */
export function sourceTextParts(source: AssistantSourceResolution, message: { id: string; text: string }) {
    const anchor = assistantSourceAnchor(source);
    const locator = source.locator;
    if (!anchor || message.id !== anchor.message || !locator || !source.content) return null;
    const bytes = new TextEncoder().encode(message.text);
    if (locator.byte_end > bytes.length) return null;
    try {
        const decoder = new TextDecoder('utf-8', { fatal: true });
        const match = decoder.decode(bytes.subarray(locator.byte_start, locator.byte_end));
        if (match !== source.content) return null;
        return { before: decoder.decode(bytes.subarray(0, locator.byte_start)), match,
            after: decoder.decode(bytes.subarray(locator.byte_end)) };
    } catch { return null; }
}

