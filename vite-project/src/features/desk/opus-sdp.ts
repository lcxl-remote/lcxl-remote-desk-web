type SdpLine = {
    text: string;
    ending: string;
};

function splitLines(sdp: string): SdpLine[] | null {
    if (sdp.replaceAll('\r\n', '').includes('\r')) {
        return null;
    }

    const lines: SdpLine[] = [];
    const matcher = /([^\r\n]*)(\r\n|\n|$)/g;
    let match: RegExpExecArray | null;

    while ((match = matcher.exec(sdp)) !== null && match[0] !== '') {
        lines.push({ text: match[1], ending: match[2] });
    }

    return lines;
}

function normalizeFmtpValue(value: string): string {
    const parameters = value.split(';').filter((parameter) => {
        if (parameter.trim() === '') return false;
        const separator = parameter.indexOf('=');
        if (separator < 0) return true;
        const key = parameter.slice(0, separator).trim().toLowerCase();
        return key !== 'stereo';
    });

    parameters.push('stereo=1');
    return parameters.join(';');
}

export function normalizeOpusStereoSdp<T extends string | null | undefined>(sdp: T): T;
export function normalizeOpusStereoSdp(
    sdp: string | null | undefined,
): string | null | undefined {
    if (typeof sdp !== 'string' || sdp.length === 0) return sdp;

    try {
        const lines = splitLines(sdp);
        if (!lines) return sdp;

        const defaultEnding = lines.find((line) => line.ending !== '')?.ending ?? '\r\n';
        let changed = false;

        for (let sectionStart = 0; sectionStart < lines.length;) {
            if (!lines[sectionStart].text.startsWith('m=')) {
                sectionStart += 1;
                continue;
            }

            let sectionEnd = sectionStart + 1;
            while (sectionEnd < lines.length && !lines[sectionEnd].text.startsWith('m=')) {
                sectionEnd += 1;
            }

            if (!/^m=audio(?:\s|$)/i.test(lines[sectionStart].text)) {
                sectionStart = sectionEnd;
                continue;
            }

            const opusPayloads = new Map<string, number>();
            for (let index = sectionStart + 1; index < sectionEnd; index += 1) {
                const match = lines[index].text.match(
                    /^a=rtpmap:(\d+)\s+opus\/48000\/2(?:\s|$)/i,
                );
                if (match) opusPayloads.set(match[1], index);
            }

            const orderedPayloads = [...opusPayloads.entries()].sort(
                ([, leftIndex], [, rightIndex]) => rightIndex - leftIndex,
            );
            for (const [payloadType, rtpmapIndex] of orderedPayloads) {
                const fmtpIndexes: number[] = [];
                for (let index = sectionStart + 1; index < sectionEnd; index += 1) {
                    const match = lines[index].text.match(/^a=fmtp:(\d+)\s+(.*)$/i);
                    if (match?.[1] === payloadType) fmtpIndexes.push(index);
                }

                if (fmtpIndexes.length > 1) return sdp;

                if (fmtpIndexes.length === 1) {
                    const index = fmtpIndexes[0];
                    const match = lines[index].text.match(/^(a=fmtp:\d+)(\s+)(.*)$/i);
                    if (!match) return sdp;
                    const normalized = `${match[1]}${match[2]}${normalizeFmtpValue(match[3])}`;
                    if (normalized !== lines[index].text) {
                        lines[index].text = normalized;
                        changed = true;
                    }
                    continue;
                }

                const originalEnding = lines[rtpmapIndex].ending;
                if (originalEnding === '') {
                    lines[rtpmapIndex].ending = defaultEnding;
                }
                lines.splice(rtpmapIndex + 1, 0, {
                    text: `a=fmtp:${payloadType} stereo=1`,
                    ending: originalEnding,
                });
                sectionEnd += 1;
                changed = true;
            }

            sectionStart = sectionEnd;
        }

        if (!changed) return sdp;
        return lines.map((line) => `${line.text}${line.ending}`).join('');
    } catch {
        return sdp;
    }
}
