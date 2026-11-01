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

it('accepts service policies above previous client ceilings while retaining scope consistency', () => {
    const scope = { max_count: 80, max_bytes_per_attachment: 96 * 1024 * 1024, max_total_bytes: 512 * 1024 * 1024,
        media_types: Array.from({ length: 40 }, (_, index) => `application/x-type-${index}`) };
    expect(validAttachmentPolicy({ automatic: scope, approval_ceiling: scope })).toBe(true);
    expect(validAttachmentPolicy({ automatic: { ...scope, max_total_bytes: 1 }, approval_ceiling: scope })).toBe(false);
});
