import { describe, expect, it } from 'vitest';
import { validAttachmentPolicy } from './attachment-policy';

describe('attachment policy bounds', () => {
    const limits = (bytes: number, type = 'text/plain') => ({ max_count: 1, max_bytes_per_attachment: bytes, max_total_bytes: bytes, media_types: [type] });
    it('requires automatic limits and media types within the hard ceiling', () => {
        expect(validAttachmentPolicy({ automatic: limits(4), approval_ceiling: limits(8) })).toBe(true);
        expect(validAttachmentPolicy({ automatic: limits(9), approval_ceiling: limits(8) })).toBe(false);
        expect(validAttachmentPolicy({ automatic: limits(4), approval_ceiling: limits(8, 'image/png') })).toBe(false);
    });
    it('accepts the exact edge text type and rejects wildcards and fractions', () => {
        expect(validAttachmentPolicy({ automatic: limits(4, 'text/plain;charset=utf-8'), approval_ceiling: limits(8, 'text/plain;charset=utf-8') })).toBe(true);
        expect(validAttachmentPolicy({ automatic: limits(4, 'text/*'), approval_ceiling: limits(8, 'text/*') })).toBe(false);
        expect(validAttachmentPolicy({ automatic: limits(1.5), approval_ceiling: limits(8) })).toBe(false);
    });
});
