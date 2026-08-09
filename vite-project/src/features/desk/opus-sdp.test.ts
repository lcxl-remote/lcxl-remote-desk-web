import { describe, expect, it } from 'vitest';
import { normalizeOpusStereoSdp } from './opus-sdp';

const LIVE_OFFER = [
    'v=0',
    'm=audio 9 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126',
    'a=mid:1',
    'a=sendrecv',
    'a=rtpmap:111 opus/48000/2',
    'a=rtcp-fb:111 transport-cc',
    'a=fmtp:111 minptime=10;useinbandfec=1',
    'a=rtpmap:63 red/48000/2',
    'a=fmtp:63 111/111',
    '',
].join('\r\n');

describe('normalizeOpusStereoSdp', () => {
    it('adds stereo to the live Opus payload and leaves RED unchanged', () => {
        const normalized = normalizeOpusStereoSdp(LIVE_OFFER);

        expect(normalized).toContain(
            'a=fmtp:111 minptime=10;useinbandfec=1;stereo=1\r\n',
        );
        expect(normalized).toContain('a=fmtp:63 111/111\r\n');
        expect(normalized.match(/(?:^|;)stereo=1(?:;|\r?$)/gm)).toHaveLength(1);
    });

    it('finds a dynamic Opus payload within its audio section', () => {
        const input = [
            'v=0',
            'm=video 9 UDP/TLS/RTP/SAVPF 109',
            'a=rtpmap:109 VP8/90000',
            'a=fmtp:109 max-fs=12288',
            'm=audio 9 UDP/TLS/RTP/SAVPF 109',
            'a=rtpmap:109 OPUS/48000/2',
            'a=fmtp:109 minptime=10',
        ].join('\n');

        expect(normalizeOpusStereoSdp(input)).toContain(
            'a=fmtp:109 minptime=10;stereo=1',
        );
        expect(normalizeOpusStereoSdp(input)).toContain('a=fmtp:109 max-fs=12288');
    });

    it('replaces only exact stereo keys and preserves sprop-stereo tokens', () => {
        for (const sprop of ['sprop-stereo=0', 'sprop-stereo=1']) {
            const input = LIVE_OFFER.replace(
                'minptime=10;useinbandfec=1',
                `minptime=10;${sprop};STEREO=0;stereo=1`,
            );
            const normalized = normalizeOpusStereoSdp(input);

            expect(normalized).toContain(`minptime=10;${sprop};stereo=1`);
            expect(normalized).not.toContain('STEREO=0');
        }
    });

    it('creates an Opus fmtp line when one is absent', () => {
        const input = LIVE_OFFER.replace(
            'a=fmtp:111 minptime=10;useinbandfec=1\r\n',
            '',
        );

        expect(normalizeOpusStereoSdp(input)).toContain(
            'a=rtpmap:111 opus/48000/2\r\na=fmtp:111 stereo=1\r\n',
        );
    });

    it('normalizes an empty fmtp value without creating a leading separator', () => {
        const input = LIVE_OFFER.replace(
            'a=fmtp:111 minptime=10;useinbandfec=1',
            'a=fmtp:111 ',
        );

        expect(normalizeOpusStereoSdp(input)).toContain('a=fmtp:111 stereo=1\r\n');
        expect(normalizeOpusStereoSdp(input)).not.toContain('a=fmtp:111 ;stereo=1');
    });

    it('preserves CRLF, LF, trailing-newline semantics, and idempotence', () => {
        const crlf = normalizeOpusStereoSdp(LIVE_OFFER);
        expect(crlf.replaceAll('\r\n', '')).not.toContain('\n');
        expect(crlf.endsWith('\r\n')).toBe(true);
        expect(normalizeOpusStereoSdp(crlf)).toBe(crlf);

        const lfWithoutTrailingNewline = LIVE_OFFER.replaceAll('\r\n', '\n').slice(0, -1);
        const normalizedLf = normalizeOpusStereoSdp(lfWithoutTrailingNewline);
        expect(normalizedLf).not.toContain('\r');
        expect(normalizedLf.endsWith('\n')).toBe(false);
        expect(normalizeOpusStereoSdp(normalizedLf)).toBe(normalizedLf);
    });

    it('returns unsupported or unsafe input unchanged', () => {
        const noAudio = 'v=0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n';
        const noOpus = 'v=0\r\nm=audio 9 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n';
        const duplicateFmtp = `${LIVE_OFFER}a=fmtp:111 stereo=0\r\n`;
        const loneCarriageReturn = LIVE_OFFER.replace('\r\n', '\r');

        expect(normalizeOpusStereoSdp(noAudio)).toBe(noAudio);
        expect(normalizeOpusStereoSdp(noOpus)).toBe(noOpus);
        expect(normalizeOpusStereoSdp(duplicateFmtp)).toBe(duplicateFmtp);
        expect(normalizeOpusStereoSdp(loneCarriageReturn)).toBe(loneCarriageReturn);
        expect(normalizeOpusStereoSdp('')).toBe('');
        expect(normalizeOpusStereoSdp(null)).toBeNull();
        expect(normalizeOpusStereoSdp(undefined)).toBeUndefined();
    });
});
