/**
 * Diagnostics for the file manager's WebRTC leg.
 *
 * A data channel that never opens otherwise surfaces as a bare timeout, and the
 * facts that separate the plausible causes — which ICE servers the central
 * actually handed out, which candidate types the browser managed to gather,
 * where negotiation stopped — are only visible in `chrome://webrtc-internals`.
 * This module collects those few facts while the connection is being set up and
 * turns them into something the page can show and the user can paste into a
 * report.
 *
 * The single most diagnostic fact is the relay count: a TURN server in the list
 * paired with zero `relay` candidates means the browser cannot reach the relay,
 * which is by far the most common cause of a stalled gathering phase.
 *
 * Nothing secret is retained. ICE server entries keep their URL only — never the
 * TURN username or credential — and candidates are counted by type rather than
 * stored, so neither private addresses nor mDNS host names leave the page.
 */

/** Candidate types as they appear in the `typ` field of a candidate line. */
export type IceCandidateType = 'host' | 'srflx' | 'prflx' | 'relay' | 'unknown';

/** Every counted type, in display order. */
export const ICE_CANDIDATE_TYPES: readonly IceCandidateType[] = [
    'host',
    'srflx',
    'prflx',
    'relay',
    'unknown',
] as const;

/** The two stages a connection attempt passes through, timed separately. */
export type ConnectionStage = 'session' | 'dataChannel';

/** Upper bound on retained ICE server URLs, so a pathological list cannot grow
 * the snapshot without bound. */
const MAX_ICE_SERVER_URLS = 12;

export interface ConnectionDiagnostics {
    /** Credential-free ICE server URLs the peer connection was configured with. */
    iceServerUrls: string[];
    /** How many candidates of each type were gathered. */
    candidateCounts: Record<IceCandidateType, number>;
    /** Terminal `RTCPeerConnection.iceGatheringState`, when observed. */
    gatheringState: string | null;
    /** Terminal `RTCPeerConnection.iceConnectionState`, when observed. */
    iceConnectionState: string | null;
    /** Milliseconds spent establishing the signaling session, once it settled. */
    sessionMs: number | null;
    /** Milliseconds spent establishing the data channel, once it settled. */
    dataChannelMs: number | null;
    /** The stage that failed, or `null` while nothing has failed. */
    failedStage: ConnectionStage | null;
}

function emptyCounts(): Record<IceCandidateType, number> {
    return { host: 0, srflx: 0, prflx: 0, relay: 0, unknown: 0 };
}

/**
 * The URLs of an `ice_servers` payload, with credentials dropped.
 *
 * The payload is whatever the host and the central put on the wire, so it is
 * parsed defensively: anything that is not a string URL is skipped rather than
 * throwing inside a connection callback.
 */
export function sanitizeIceServerUrls(iceServers: unknown): string[] {
    if (!Array.isArray(iceServers)) return [];
    const urls: string[] = [];
    for (const server of iceServers) {
        if (!server || typeof server !== 'object') continue;
        const raw = (server as { urls?: unknown }).urls;
        const candidates = Array.isArray(raw) ? raw : [raw];
        for (const url of candidates) {
            if (typeof url !== 'string' || url.length === 0) continue;
            if (urls.includes(url)) continue;
            urls.push(url);
            if (urls.length >= MAX_ICE_SERVER_URLS) return urls;
        }
    }
    return urls;
}

/** Whether the list contains a relay (TURN) server, which is what makes a zero
 * relay count meaningful rather than expected. */
export function hasTurnServer(urls: readonly string[]): boolean {
    return urls.some((url) => /^turns?:/i.test(url));
}

/**
 * The `typ` of a candidate line.
 *
 * Only the type is read; the rest of the line (addresses, ports, mDNS names) is
 * deliberately never retained.
 */
export function candidateTypeOf(candidate: string | null | undefined): IceCandidateType {
    if (!candidate) return 'unknown';
    const matched = /(?:^|\s)typ\s+(\w+)/.exec(candidate);
    const typ = matched?.[1];
    switch (typ) {
        case 'host':
        case 'srflx':
        case 'prflx':
        case 'relay':
            return typ;
        default:
            return 'unknown';
    }
}

/** i18n keys for the conclusion the snapshot points to. */
export const DIAGNOSIS_KEYS = {
    noIceServers: 'pages.fileManager.diagnostics.noIceServers',
    noRelayCandidates: 'pages.fileManager.diagnostics.noRelayCandidates',
    gatheringStalled: 'pages.fileManager.diagnostics.gatheringStalled',
    negotiationFailed: 'pages.fileManager.diagnostics.negotiationFailed',
    inconclusive: 'pages.fileManager.diagnostics.inconclusive',
} as const;

/**
 * The most likely cause of a failed attempt, as an i18n key.
 *
 * Ordered most-specific first: a configured TURN server that produced no relay
 * candidate is a far sharper finding than "gathering did not finish", and would
 * be masked by it since a browser that cannot reach the relay also keeps
 * gathering until its allocation attempts time out.
 */
export function diagnosisKey(diagnostics: ConnectionDiagnostics): string {
    if (diagnostics.iceServerUrls.length === 0) return DIAGNOSIS_KEYS.noIceServers;
    if (hasTurnServer(diagnostics.iceServerUrls) && diagnostics.candidateCounts.relay === 0) {
        return DIAGNOSIS_KEYS.noRelayCandidates;
    }
    if (diagnostics.gatheringState !== null && diagnostics.gatheringState !== 'complete') {
        return DIAGNOSIS_KEYS.gatheringStalled;
    }
    if (diagnostics.iceConnectionState === 'failed') return DIAGNOSIS_KEYS.negotiationFailed;
    return DIAGNOSIS_KEYS.inconclusive;
}

/**
 * A plain-text rendering of the snapshot for the clipboard.
 *
 * The field names are stable and untranslated on purpose: this block is a
 * machine-readable artifact meant to be pasted into a bug report or handed to
 * whoever runs the relay, not page copy. The localized text lives in the panel
 * that renders around it.
 */
export function formatDiagnostics(diagnostics: ConnectionDiagnostics): string {
    const counts = ICE_CANDIDATE_TYPES.map(
        (type) => `${type}=${diagnostics.candidateCounts[type]}`,
    ).join(' ');
    const lines = [
        `failed_stage: ${diagnostics.failedStage ?? 'none'}`,
        `ice_servers: ${diagnostics.iceServerUrls.length > 0 ? diagnostics.iceServerUrls.join(', ') : 'none'}`,
        `candidates: ${counts}`,
        `ice_gathering_state: ${diagnostics.gatheringState ?? 'unknown'}`,
        `ice_connection_state: ${diagnostics.iceConnectionState ?? 'unknown'}`,
        `session_ms: ${diagnostics.sessionMs ?? 'n/a'}`,
        `data_channel_ms: ${diagnostics.dataChannelMs ?? 'n/a'}`,
    ];
    return lines.join('\n');
}

/** Mutable collector fed by the connection callbacks. */
export interface DiagnosticsCollector {
    /** Record the ICE server list a peer connection is about to be built with. */
    noteIceServers(iceServers: unknown): void;
    /** Count one locally gathered candidate. */
    noteCandidate(candidate: string | null | undefined): void;
    /** Record the currently observed peer-connection states. */
    noteStates(gatheringState: string | null, iceConnectionState: string | null): void;
    /** Start timing a stage. */
    startStage(stage: ConnectionStage): void;
    /** Stop timing a stage that succeeded. */
    endStage(stage: ConnectionStage): void;
    /** Stop timing a stage that failed, and remember which one it was. */
    failStage(stage: ConnectionStage): void;
    /** Forget the peer-connection half, keeping the session facts, so a retry
     * does not report the previous attempt's candidates as its own. */
    resetDataChannel(): void;
    /** An immutable copy of everything collected so far. */
    snapshot(): ConnectionDiagnostics;
}

export function createDiagnosticsCollector(now: () => number = Date.now): DiagnosticsCollector {
    let iceServerUrls: string[] = [];
    let candidateCounts = emptyCounts();
    let gatheringState: string | null = null;
    let iceConnectionState: string | null = null;
    let sessionMs: number | null = null;
    let dataChannelMs: number | null = null;
    let failedStage: ConnectionStage | null = null;
    const startedAt: Partial<Record<ConnectionStage, number>> = {};

    const settle = (stage: ConnectionStage) => {
        const started = startedAt[stage];
        if (started === undefined) return;
        const elapsed = now() - started;
        if (stage === 'session') sessionMs = elapsed;
        else dataChannelMs = elapsed;
        delete startedAt[stage];
    };

    return {
        noteIceServers(iceServers) {
            iceServerUrls = sanitizeIceServerUrls(iceServers);
        },
        noteCandidate(candidate) {
            candidateCounts[candidateTypeOf(candidate)] += 1;
        },
        noteStates(gathering, iceConnection) {
            if (gathering !== null) gatheringState = gathering;
            if (iceConnection !== null) iceConnectionState = iceConnection;
        },
        startStage(stage) {
            startedAt[stage] = now();
            if (failedStage === stage) failedStage = null;
        },
        endStage(stage) {
            settle(stage);
        },
        failStage(stage) {
            settle(stage);
            failedStage = stage;
        },
        resetDataChannel() {
            candidateCounts = emptyCounts();
            gatheringState = null;
            iceConnectionState = null;
            dataChannelMs = null;
            iceServerUrls = [];
            delete startedAt.dataChannel;
        },
        snapshot() {
            return {
                iceServerUrls: [...iceServerUrls],
                candidateCounts: { ...candidateCounts },
                gatheringState,
                iceConnectionState,
                sessionMs,
                dataChannelMs,
                failedStage,
            };
        },
    };
}
