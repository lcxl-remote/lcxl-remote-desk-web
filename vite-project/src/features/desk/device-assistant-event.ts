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
    answer?: string | null;
    provenance?: AiProvenance | null;
};
