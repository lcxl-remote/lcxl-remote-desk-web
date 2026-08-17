import { describe, expect, it } from 'vitest';
import {
    DIAGNOSIS_KEYS,
    candidateTypeOf,
    createDiagnosticsCollector,
    diagnosisKey,
    formatDiagnostics,
    hasTurnServer,
    sanitizeIceServerUrls,
} from './connection-diagnostics';

// The collector is what turns a failed connection into something a user can act
// on, so what it keeps — and just as importantly what it refuses to keep —
// is asserted here directly. These are pure functions, so unlike the hook tests
// this file is free to hold many cases.

describe('sanitizeIceServerUrls', () => {
    it('keeps URLs and drops credentials', () => {
        const urls = sanitizeIceServerUrls([
            { urls: 'stun:stun.example:3478' },
            { urls: ['turn:relay.example:3478', 'turns:relay.example:5349'], username: 'alice', credential: 's3cret' },
        ]);
        expect(urls).toEqual([
            'stun:stun.example:3478',
            'turn:relay.example:3478',
            'turns:relay.example:5349',
        ]);
        // Nothing that could be a credential survives anywhere in the output.
        expect(JSON.stringify(urls)).not.toContain('alice');
        expect(JSON.stringify(urls)).not.toContain('s3cret');
    });

    it('tolerates whatever the wire actually carries', () => {
        expect(sanitizeIceServerUrls(undefined)).toEqual([]);
        expect(sanitizeIceServerUrls('nonsense')).toEqual([]);
        expect(sanitizeIceServerUrls([null, 42, { urls: 7 }, { urls: [''] }])).toEqual([]);
    });

    it('deduplicates and bounds the list', () => {
        expect(sanitizeIceServerUrls([{ urls: 'stun:a' }, { urls: 'stun:a' }])).toEqual(['stun:a']);
        const many = Array.from({ length: 40 }, (_, i) => ({ urls: `stun:host-${i}` }));
        expect(sanitizeIceServerUrls(many)).toHaveLength(12);
    });
});

describe('candidateTypeOf', () => {
    it('reads the type and nothing else', () => {
        expect(candidateTypeOf('candidate:1 1 udp 2113937151 192.168.1.5 50000 typ host generation 0')).toBe('host');
        expect(candidateTypeOf('candidate:2 1 udp 1677721 203.0.113.7 50001 typ srflx raddr 0.0.0.0')).toBe('srflx');
        expect(candidateTypeOf('candidate:3 1 udp 41885 198.51.100.9 50002 typ relay raddr 0.0.0.0')).toBe('relay');
        expect(candidateTypeOf('candidate:4 1 udp 1 1.2.3.4 5 typ prflx')).toBe('prflx');
        expect(candidateTypeOf('garbage')).toBe('unknown');
        expect(candidateTypeOf(null)).toBe('unknown');
    });
});

describe('hasTurnServer', () => {
    it('recognizes both TURN schemes and ignores STUN', () => {
        expect(hasTurnServer(['stun:stun.example:3478'])).toBe(false);
        expect(hasTurnServer(['turn:relay.example:3478'])).toBe(true);
        expect(hasTurnServer(['TURNS:relay.example:5349'])).toBe(true);
        expect(hasTurnServer([])).toBe(false);
    });
});

describe('diagnosisKey', () => {
    const base = {
        iceServerUrls: [] as string[],
        candidateCounts: { host: 0, srflx: 0, prflx: 0, relay: 0, unknown: 0 },
        gatheringState: null as string | null,
        iceConnectionState: null as string | null,
        sessionMs: null as number | null,
        dataChannelMs: null as number | null,
        failedStage: 'dataChannel' as const,
    };

    it('reports an empty ICE server list first', () => {
        expect(diagnosisKey(base)).toBe(DIAGNOSIS_KEYS.noIceServers);
    });

    it('reports an unreachable relay when TURN produced no relay candidate', () => {
        // The production signature: a relay is configured, host candidates were
        // gathered, and gathering never finished because the relay never
        // answered. The relay finding must win over the stalled-gathering one,
        // which is merely its symptom.
        expect(diagnosisKey({
            ...base,
            iceServerUrls: ['turn:relay.example:3478'],
            candidateCounts: { ...base.candidateCounts, host: 2 },
            gatheringState: 'gathering',
        })).toBe(DIAGNOSIS_KEYS.noRelayCandidates);
    });

    it('reports a stalled gathering when no relay was configured', () => {
        expect(diagnosisKey({
            ...base,
            iceServerUrls: ['stun:stun.example:3478'],
            gatheringState: 'gathering',
        })).toBe(DIAGNOSIS_KEYS.gatheringStalled);
    });

    it('reports a failed negotiation once candidates are complete', () => {
        expect(diagnosisKey({
            ...base,
            iceServerUrls: ['turn:relay.example:3478'],
            candidateCounts: { ...base.candidateCounts, host: 1, relay: 1 },
            gatheringState: 'complete',
            iceConnectionState: 'failed',
        })).toBe(DIAGNOSIS_KEYS.negotiationFailed);
    });

    it('admits when it cannot tell', () => {
        expect(diagnosisKey({
            ...base,
            iceServerUrls: ['stun:stun.example:3478'],
            gatheringState: 'complete',
        })).toBe(DIAGNOSIS_KEYS.inconclusive);
    });
});

describe('createDiagnosticsCollector', () => {
    it('counts candidates by type without retaining them', () => {
        const collector = createDiagnosticsCollector();
        collector.noteIceServers([{ urls: 'turn:relay.example:3478', credential: 's3cret' }]);
        collector.noteCandidate('candidate:1 1 udp 1 192.168.1.5 50000 typ host');
        collector.noteCandidate('candidate:2 1 udp 1 8f4e2c11-1234.local 50001 typ host');
        collector.noteCandidate('candidate:3 1 udp 1 203.0.113.7 50002 typ srflx');

        const snapshot = collector.snapshot();
        expect(snapshot.candidateCounts).toEqual({ host: 2, srflx: 1, prflx: 0, relay: 0, unknown: 0 });
        // Neither the private address nor the mDNS name is anywhere in the
        // snapshot — only counts are kept.
        const serialized = JSON.stringify(snapshot);
        expect(serialized).not.toContain('192.168.1.5');
        expect(serialized).not.toContain('.local');
        expect(serialized).not.toContain('s3cret');
    });

    it('times each stage separately and remembers which failed', () => {
        let now = 1000;
        const collector = createDiagnosticsCollector(() => now);
        collector.startStage('session');
        now = 1800;
        collector.endStage('session');
        collector.startStage('dataChannel');
        now = 21_800;
        collector.failStage('dataChannel');

        const snapshot = collector.snapshot();
        expect(snapshot.sessionMs).toBe(800);
        expect(snapshot.dataChannelMs).toBe(20_000);
        expect(snapshot.failedStage).toBe('dataChannel');
    });

    it('starts a retry from a clean data-channel slate but keeps the session facts', () => {
        let now = 0;
        const collector = createDiagnosticsCollector(() => now);
        collector.startStage('session');
        now = 500;
        collector.endStage('session');
        collector.noteIceServers([{ urls: 'turn:relay.example:3478' }]);
        collector.noteCandidate('candidate:1 1 udp 1 1.2.3.4 1 typ host');
        collector.noteStates('gathering', 'checking');

        collector.resetDataChannel();
        const snapshot = collector.snapshot();
        // A retry must not report the previous attempt's candidates as its own.
        expect(snapshot.candidateCounts.host).toBe(0);
        expect(snapshot.iceServerUrls).toEqual([]);
        expect(snapshot.gatheringState).toBeNull();
        expect(snapshot.sessionMs).toBe(500);
    });

    it('snapshots are immutable copies', () => {
        const collector = createDiagnosticsCollector();
        const first = collector.snapshot();
        collector.noteCandidate('candidate:1 1 udp 1 1.2.3.4 1 typ relay');
        expect(first.candidateCounts.relay).toBe(0);
        expect(collector.snapshot().candidateCounts.relay).toBe(1);
    });
});

describe('formatDiagnostics', () => {
    it('renders a stable block for a report', () => {
        const text = formatDiagnostics({
            iceServerUrls: ['turn:relay.example:3478'],
            candidateCounts: { host: 2, srflx: 1, prflx: 0, relay: 0, unknown: 0 },
            gatheringState: 'gathering',
            iceConnectionState: 'checking',
            sessionMs: 780,
            dataChannelMs: 20_000,
            failedStage: 'dataChannel',
        });
        expect(text).toContain('failed_stage: dataChannel');
        expect(text).toContain('ice_servers: turn:relay.example:3478');
        expect(text).toContain('relay=0');
        expect(text).toContain('ice_gathering_state: gathering');
        expect(text).toContain('session_ms: 780');
    });
});
