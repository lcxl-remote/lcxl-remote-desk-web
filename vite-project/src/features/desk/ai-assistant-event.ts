import type { ContentRef } from '@/services/types';
import type { AiProvenance } from '@/components/ai-generated-mark';

export type AiAssistantEventKind =
    | 'status'
    | 'partial'
    | 'partial_committed'
    | 'retracted'
    | 'error'
    | 'turn_started'
    | 'tool_started'
    | 'tool_finished'
    | 'visual_evidence'
    | 'document_preview'
    | 'permission_required'
    | 'answer';

export type AiAssistantEvent = {
    request_id: string;
    seq: number;
    kind: AiAssistantEventKind;
    turn_id?: string | null;
    status?: string | null;
    partial_summary?: string | null;
    error?: { message?: string | null; error_code?: number | null } | null;
    tool_name?: string | null;
    tool_arguments_json?: string | null;
    tool_call_id?: string | null;
    tool_ok?: boolean | null;
    tool_output?: string | null;
    visual_evidence?: AiAssistantVisualEvidence | null;
    document_preview?: AiAssistantDocumentPreview | null;
    answer?: string | null;
    provenance?: AiProvenance | null;
};

export type AiAssistantDocumentWarning = {
    code: string;
    page?: number | null;
    detail: string;
};

export type AiAssistantDocumentPreview = {
    loading?: boolean;
    error?: string | null;
    descriptor: {
        preview_id: string;
        source_digest_sha256: string;
        page_count: number;
        template_version: string;
        font_set_sha256: string;
        engine: string;
        warnings: AiAssistantDocumentWarning[];
    };
    page: {
        preview_id: string;
        page: number;
        page_count: number;
        width: number;
        height: number;
        pixels_per_point_milli: number;
    };
    preview_data_url: string;
};

export type AiAssistantDocumentPreviewPageResponse = {
    conversation_id: string;
    preview_id: string;
    page: number;
    frame?: {
        page: AiAssistantDocumentPreview['page'];
        preview_data_url: string;
    } | null;
    error?: string | null;
};

export type AiAssistantVisualEvidence = {
    content?: ContentRef | null;
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
    frame_observation?: {
        received_at_unix_ms: number;
        receipt_age_ms: number;
        source_timestamp_ns?: number | null;
        freshness: 'fresh' | 'latest_observed' | 'unchanged_verified';
    } | null;
    expires_at_unix_ms?: number | null;
    device_id: string;
    display_summary?: string | null;
    application_summary?: string | null;
    digest_sha256?: string | null;
    size_bytes: number;
    media_type?: string | null;
    preview_data_url?: string | null;
};
