export const SCHEMA_VERSION = 1;
export const MAX_MESSAGE_BYTES = 24 * 1024 * 1024;
export const MAX_FIELDS = 64;
export const MAX_FIELD_BYTES = 64 * 1024;
export const MAX_ELEMENTS = 512;

const ACTION_KEYS = Object.freeze({
    open_page: ["action", "target"],
    navigate_page: ["action", "page", "target"],
    take_snapshot: ["action", "page", "max_elements"],
    wait_for: ["action", "page", "element", "state", "timeout_ms"],
    fill_form: ["action", "page", "fields", "mutation_class"],
    upload_file: [
        "action",
        "page",
        "element",
        "file_name",
        "media_type",
        "size_bytes",
        "digest_sha256",
        "bytes_base64",
        "mutation_class"
    ],
    fill_form_and_upload: [
        "action",
        "page",
        "fields",
        "upload_element",
        "file_name",
        "media_type",
        "size_bytes",
        "digest_sha256",
        "bytes_base64",
        "mutation_class"
    ],
    activate_element: ["action", "page", "element", "activation_class"]
});

function exactKeys(value, expected) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
        return false;
    }
    const actual = Object.keys(value).sort();
    const wanted = [...expected].sort();
    return actual.length === wanted.length && actual.every((key, index) => key === wanted[index]);
}

function boundedString(value, maximum) {
    return typeof value === "string" && value.length > 0 && new TextEncoder().encode(value).length <= maximum;
}

function validateTarget(target) {
    if (!exactKeys(target, ["url", "origin"]) || !boundedString(target.url, 4096)) {
        throw new Error("invalid_navigation_target");
    }
    const parsed = new URL(target.url);
    if (parsed.username || parsed.password || parsed.hash) {
        throw new Error("invalid_navigation_target");
    }
    if (parsed.protocol !== "https:" && !(parsed.protocol === "http:" && ["127.0.0.1", "localhost", "::1"].includes(parsed.hostname))) {
        throw new Error("invalid_navigation_target");
    }
    const expectedPort = parsed.port || (parsed.protocol === "https:" ? "443" : "80");
    if (
        !target.origin ||
        target.origin.host_ascii !== parsed.hostname.toLowerCase() ||
        String(target.origin.port) !== expectedPort
    ) {
        throw new Error("origin_mismatch");
    }
}

function validateElement(element) {
    if (!element || !boundedString(element.element_id, 256) || !boundedString(element.page_id, 256)) {
        throw new Error("invalid_element_ref");
    }
    if (!Number.isSafeInteger(element.document_revision) || element.document_revision < 1) {
        throw new Error("invalid_element_ref");
    }
}

function validatePage(page) {
    if (
        !page ||
        !boundedString(page.page_id, 256) ||
        !boundedString(page.page_incarnation, 256) ||
        !Number.isSafeInteger(page.document_revision) ||
        page.document_revision < 1
    ) {
        throw new Error("invalid_page_ref");
    }
}

function validateFields(fields) {
    if (!Array.isArray(fields) || fields.length < 1 || fields.length > MAX_FIELDS) {
        throw new Error("invalid_form_fields");
    }
    const ids = new Set();
    for (const field of fields) {
        if (!exactKeys(field, ["element", "value"]) || typeof field.value !== "string") {
            throw new Error("invalid_form_fields");
        }
        validateElement(field.element);
        if (new TextEncoder().encode(field.value).length > MAX_FIELD_BYTES || ids.has(field.element.element_id)) {
            throw new Error("invalid_form_fields");
        }
        ids.add(field.element.element_id);
    }
}

function validateUpload(action, elementKey) {
    validateElement(action[elementKey]);
    if (
        !boundedString(action.file_name, 200) ||
        /[\\/:*?"<>|]/u.test(action.file_name) ||
        !boundedString(action.media_type, 256) ||
        !Number.isSafeInteger(action.size_bytes) ||
        action.size_bytes < 1 ||
        action.size_bytes > 64 * 1024 * 1024 ||
        !/^[0-9a-f]{64}$/u.test(action.digest_sha256) ||
        !boundedString(action.bytes_base64, Math.ceil(action.size_bytes / 3) * 4 + 8)
    ) {
        throw new Error("invalid_upload");
    }
}

function validateActivationClass(value) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
        throw new Error("invalid_activation_class");
    }
    if (["write_external_draft", "input_fallback"].includes(value.kind)) {
        if (!exactKeys(value, ["kind"])) throw new Error("invalid_activation_class");
        return;
    }
    if (
        value.kind === "send_external" &&
        exactKeys(value, [
            "kind", "payload_sha256", "snapshot_id", "idempotency_key", "site",
            "fields", "attachment_file_names"
        ]) &&
        /^[0-9a-f]{64}$/u.test(value.payload_sha256) &&
        boundedString(value.snapshot_id, 256) &&
        value.idempotency_key === `send:v1:${value.payload_sha256}` &&
        ["gmail_web", "slack_web"].includes(value.site) &&
        Array.isArray(value.attachment_file_names) &&
        value.attachment_file_names.length <= 1
    ) {
        validateFields(value.fields);
        for (const fileName of value.attachment_file_names) {
            if (!boundedString(fileName, 200) || /[\\/]/u.test(fileName)) {
                throw new Error("invalid_activation_class");
            }
        }
        if ((value.site === "gmail_web" && value.fields.length !== 3) ||
            (value.site === "slack_web" && (value.fields.length !== 1 || value.attachment_file_names.length !== 0))) {
            throw new Error("invalid_activation_class");
        }
        return;
    }
    throw new Error("invalid_activation_class");
}

export function parseHostCommand(raw) {
    if (typeof raw === "string" && new TextEncoder().encode(raw).length > MAX_MESSAGE_BYTES) {
        throw new Error("message_too_large");
    }
    const value = typeof raw === "string" ? JSON.parse(raw) : raw;
    if (!exactKeys(value, ["schema_version", "type", "request_id", "action"])) {
        throw new Error("invalid_command_envelope");
    }
    if (value.schema_version !== SCHEMA_VERSION || value.type !== "request" || !boundedString(value.request_id, 256)) {
        throw new Error("invalid_command_envelope");
    }
    const kind = value.action?.action;
    const allowedKeys = ACTION_KEYS[kind];
    if (!allowedKeys || !exactKeys(value.action, allowedKeys)) {
        throw new Error("unsupported_action");
    }
    switch (kind) {
        case "open_page":
            validateTarget(value.action.target);
            break;
        case "navigate_page":
            validatePage(value.action.page);
            validateTarget(value.action.target);
            break;
        case "take_snapshot":
            validatePage(value.action.page);
            if (!Number.isInteger(value.action.max_elements) || value.action.max_elements < 1 || value.action.max_elements > MAX_ELEMENTS) {
                throw new Error("invalid_snapshot_bound");
            }
            break;
        case "wait_for":
            validatePage(value.action.page);
            validateElement(value.action.element);
            if (!Number.isInteger(value.action.timeout_ms) || value.action.timeout_ms < 1 || value.action.timeout_ms > 30000) {
                throw new Error("invalid_wait_bound");
            }
            break;
        case "fill_form":
            validatePage(value.action.page);
            validateFields(value.action.fields);
            break;
        case "upload_file":
            validatePage(value.action.page);
            validateUpload(value.action, "element");
            break;
        case "fill_form_and_upload":
            validatePage(value.action.page);
            validateFields(value.action.fields);
            validateUpload(value.action, "upload_element");
            break;
        case "activate_element":
            validatePage(value.action.page);
            validateElement(value.action.element);
            validateActivationClass(value.action.activation_class);
            break;
        default:
            throw new Error("unsupported_action");
    }
    return value;
}

export function response(requestId, ok, result, errorCode) {
    return {
        schema_version: SCHEMA_VERSION,
        type: "response",
        request_id: requestId,
        ok,
        result: ok ? result : null,
        error_code: ok ? null : errorCode
    };
}
