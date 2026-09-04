import type { AiProvenance } from '@/components/ai-generated-mark';

export type DeviceAssistantEventKind =
    | 'status'
    | 'partial'
    | 'partial_committed'
    | 'retracted'
    | 'error'
    | 'turn_started'
    | 'tool_started'
    | 'tool_finished'
    | 'visual_evidence'
    | 'permission_required'
    | 'answer';

export type DeviceAssistantEvent = {
    request_id: string;
    seq: number;
    kind: DeviceAssistantEventKind;
    status?: string | null;
    partial_summary?: string | null;
    error?: { message?: string | null } | null;
    tool_name?: string | null;
    tool_arguments_json?: string | null;
    tool_call_id?: string | null;
    tool_ok?: boolean | null;
    tool_output?: string | null;
    visual_evidence?: DeviceAssistantVisualEvidence | null;
    answer?: string | null;
    provenance?: AiProvenance | null;
};

export type DeviceAssistantVisualEvidence = {
    schema_version: number;
    evidence_id: string;
    conversation_id: string;
    focus_input_revision: number;
    turn_id: string;
    tool_call_id: string;
    frame_id: string;
    phase: 'before' | 'observation' | 'after';
    status: 'available' | 'expired' | 'not_retained' | 'failed' | 'blocked';
    captured_at_unix_ms: number;
    expires_at_unix_ms?: number | null;
    device_id: string;
    display_summary?: string | null;
    application_summary?: string | null;
    digest_sha256?: string | null;
    size_bytes: number;
    media_type?: string | null;
    preview_data_url?: string | null;
};
