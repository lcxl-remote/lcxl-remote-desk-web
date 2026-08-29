(() => {
    if (globalThis.__lcxlBrowserAssistantLoaded) {
        return;
    }
    globalThis.__lcxlBrowserAssistantLoaded = true;
    const documentIncarnation = crypto.randomUUID();
    const MAX_ACCESSIBLE_NAME_BYTES = 1024;
    const MAX_FORM_VALUE_BYTES = 64 * 1024;
    const MAX_SNAPSHOT_TOTAL_BYTES = 256 * 1024;
    const textEncoder = new TextEncoder();
    let documentRevision = 1;
    let elements = new Map();
    let elementIds = new WeakMap();

    function boundedText(value, maximum = 1024) {
        const normalized = String(value || "").replace(/\s+/gu, " ").trim();
        const bytes = textEncoder.encode(normalized);
        if (bytes.length <= maximum) return normalized;
        let end = maximum;
        while (end > 0 && (bytes[end] & 0xc0) === 0x80) end -= 1;
        return new TextDecoder().decode(bytes.slice(0, end));
    }

    function roleOf(element) {
        const role = element.getAttribute("role")?.toLowerCase();
        const tag = element.tagName.toLowerCase();
        const type = element.getAttribute("type")?.toLowerCase();
        if (role === "button" || tag === "button") return "button";
        if (role === "link" || tag === "a") return "link";
        if (role === "checkbox" || type === "checkbox") return "checkbox";
        if (role === "combobox" || tag === "select") return "combobox";
        if (role === "option" || tag === "option") return "option";
        if (role === "tab") return "tab";
        if (role === "dialog") return "dialog";
        if (tag === "input" || tag === "textarea" || element.isContentEditable) return "textbox";
        return "generic";
    }

    function isGmail() {
        return location.protocol === "https:" && location.hostname === "mail.google.com";
    }

    function isSlack() {
        return location.protocol === "https:" && (
            location.hostname === "app.slack.com" || location.hostname.endsWith(".slack.com")
        );
    }

    function accessibleName(element, role) {
        if (isGmail() && element instanceof HTMLInputElement && element.type === "file") {
            return "Gmail attachment file input";
        }
        const explicit = boundedText(
            element.getAttribute("aria-label") ||
            element.getAttribute("title") ||
            element.getAttribute("placeholder") ||
            element.getAttribute("name"),
            MAX_ACCESSIBLE_NAME_BYTES
        );
        if (explicit) return explicit;
        // Container roles can contain an entire inbox or channel. Never turn
        // their descendant text into an accessible name. Only actionable
        // controls may use their own bounded visible text as a fallback.
        if (!["button", "link", "checkbox", "combobox", "option", "tab", "textbox"].includes(role)) {
            return "";
        }
        return boundedText(element.innerText || element.textContent, MAX_ACCESSIBLE_NAME_BYTES);
    }

    function currentValue(element) {
        if (element instanceof HTMLInputElement && element.type === "password") {
            return null;
        }
        if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement) {
            return boundedText(element.value, MAX_FORM_VALUE_BYTES);
        }
        if (element.isContentEditable) {
            return boundedText(element.innerText, MAX_FORM_VALUE_BYTES);
        }
        return null;
    }

    async function sha256(value) {
        const bytes = new TextEncoder().encode(value);
        const digest = await crypto.subtle.digest("SHA-256", bytes);
        return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    }

    async function pageDescriptor() {
        const url = new URL(location.href);
        return {
            page_id: null,
            page_incarnation: documentIncarnation,
            origin: {
                kind: url.protocol === "https:" ? "https" : "http_loopback",
                host_ascii: url.hostname.toLowerCase(),
                port: Number(url.port || (url.protocol === "https:" ? 443 : 80))
            },
            document_revision: documentRevision,
            url_sha256: await sha256(url.href)
        };
    }

    async function snapshot(maxElements) {
        documentRevision += 1;
        elements = new Map();
        elementIds = new WeakMap();
        const selectors = "button,a[href],input,textarea,select,[role],[contenteditable='true']";
        const candidates = [...document.querySelectorAll(selectors)].filter((element) => {
            if (isGmail() && element instanceof HTMLInputElement && element.type === "file") {
                const compose = element.closest("[role='dialog']");
                if (!compose) return false;
                const composeRect = compose.getBoundingClientRect();
                return element.isConnected && composeRect.width > 0 && composeRect.height > 0;
            }
            const style = getComputedStyle(element);
            const rect = element.getBoundingClientRect();
            return style.visibility !== "hidden" && style.display !== "none" && rect.width > 0 && rect.height > 0;
        });
        const limit = Math.min(maxElements || 64, 512);
        const projected = [];
        let projectedBytes = 0;
        let truncated = false;
        for (const element of candidates) {
            const role = roleOf(element);
            const name = accessibleName(element, role);
            if (!name) continue;
            const value = currentValue(element);
            const elementBytes = textEncoder.encode(name).length + (value ? textEncoder.encode(value).length : 0);
            if (projected.length >= limit || projectedBytes + elementBytes > MAX_SNAPSHOT_TOTAL_BYTES) {
                truncated = true;
                break;
            }
            const elementId = registerElement(element);
            projected.push({
                element_id: elementId,
                role,
                accessible_name: name,
                value,
                element_revision: documentRevision
            });
            projectedBytes += elementBytes;
        }
        return {
            page: await pageDescriptor(),
            elements: projected,
            truncated,
            captured_at_unix_ms: Date.now()
        };
    }

    function registerElement(element) {
        const existing = elementIds.get(element);
        if (existing) return existing;
        const elementId = crypto.randomUUID();
        elementIds.set(element, elementId);
        elements.set(elementId, element);
        return elementId;
    }

    function resolveElement(reference) {
        if (reference.page_incarnation !== documentIncarnation || reference.document_revision !== documentRevision) {
            throw new Error("stale_element_ref");
        }
        const element = elements.get(reference.element_id);
        if (!element || !element.isConnected) {
            throw new Error("stale_element_ref");
        }
        return element;
    }

    function setValue(element, value) {
        if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) {
            const prototype = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
            const setter = Object.getOwnPropertyDescriptor(prototype, "value")?.set;
            setter?.call(element, value);
        } else if (element instanceof HTMLSelectElement) {
            element.value = value;
        } else if (element.isContentEditable) {
            element.focus();
            const selection = getSelection();
            const range = document.createRange();
            range.selectNodeContents(element);
            selection?.removeAllRanges();
            selection?.addRange(range);
            if (!document.execCommand("insertText", false, value)) {
                element.textContent = value;
            }
        } else {
            throw new Error("element_not_editable");
        }
        element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
        element.dispatchEvent(new Event("change", { bubbles: true }));
    }

    async function gmailCommittedRecipient(element, value) {
        if (!isGmail() || roleOf(element) !== "combobox") return null;
        const compose = element.closest("[role='dialog']");
        if (!compose) return null;
        element.dispatchEvent(new KeyboardEvent("keydown", {
            key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true
        }));
        element.dispatchEvent(new KeyboardEvent("keyup", {
            key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true
        }));
        await new Promise((resolve) => setTimeout(resolve, 100));
        const candidates = compose.querySelectorAll("[email],[data-hovercard-id]");
        for (const candidate of candidates) {
            const committed = candidate.getAttribute("email") || candidate.getAttribute("data-hovercard-id") || candidate.textContent;
            if (boundedText(committed, 64 * 1024) === value) {
                return {
                    source_element_id: registerElement(candidate),
                    container_element_id: registerElement(compose),
                    kind: "committed_text",
                    value
                };
            }
        }
        return null;
    }

    async function fill(fields) {
        const readback = [];
        for (const field of fields) {
            const element = resolveElement(field.element);
            setValue(element, field.value);
            await new Promise((resolve) => setTimeout(resolve, 50));
            const committed = await gmailCommittedRecipient(element, field.value);
            // Bind every Gmail field read-back to the same compose dialog. The
            // committed recipient already reports this container, but Subject
            // and Message Body previously returned null, making a legitimate
            // exact handoff impossible to prove at the server.
            const gmailCompose = isGmail() ? element.closest("[role='dialog']") : null;
            const containerElementId = committed?.container_element_id ||
                (gmailCompose ? registerElement(gmailCompose) : null);
            element.blur();
            readback.push({
                request_element_id: field.element.element_id,
                request_role: field.element.role,
                request_accessible_name: field.element.accessible_name,
                source_element_id: committed?.source_element_id || field.element.element_id,
                container_element_id: containerElementId,
                kind: committed?.kind || "control_value",
                value: committed?.value || currentValue(element)
            });
        }
        return readback;
    }

    function isReviewedSendControl(element) {
        if (!isGmail() && !isSlack()) return false;
        const name = accessibleName(element).toLocaleLowerCase();
        return element.getAttribute("type") === "submit" ||
            /^(send|send now|发送|发送此邮件|post|send message)$/u.test(name);
    }

    function decodeBase64(value) {
        const binary = atob(value);
        const bytes = new Uint8Array(binary.length);
        for (let index = 0; index < binary.length; index += 1) {
            bytes[index] = binary.charCodeAt(index);
        }
        return bytes;
    }

    async function upload(action, key) {
        const input = resolveElement(action[key]);
        if (!(input instanceof HTMLInputElement) || input.type !== "file") {
            throw new Error("element_not_file_input");
        }
        const bytes = decodeBase64(action.bytes_base64);
        if (bytes.byteLength !== action.size_bytes) {
            throw new Error("upload_size_mismatch");
        }
        const actualDigest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
            .map((byte) => byte.toString(16).padStart(2, "0"))
            .join("");
        if (actualDigest !== action.digest_sha256) {
            throw new Error("upload_digest_mismatch");
        }
        const file = new File([bytes], action.file_name, { type: action.media_type });
        const transfer = new DataTransfer();
        transfer.items.add(file);
        input.files = transfer.files;
        input.dispatchEvent(new Event("input", { bubbles: true }));
        input.dispatchEvent(new Event("change", { bubbles: true }));
        return action.file_name;
    }

    async function handle(action) {
        switch (action.action) {
            case "describe_page":
                return { page: await pageDescriptor() };
            case "take_snapshot":
                return { snapshot: await snapshot(action.max_elements) };
            case "fill_form":
                return { form_readback: await fill(action.fields), snapshot: await snapshot(64) };
            case "upload_file":
            {
                const attachmentFileName = await upload(action, "element");
                const captured = await snapshot(64);
                captured.elements.push({
                    element_id: crypto.randomUUID(),
                    role: "generic",
                    accessible_name: attachmentFileName,
                    value: null,
                    element_revision: captured.page.document_revision
                });
                return { attachment_file_name: attachmentFileName, snapshot: captured };
            }
            case "fill_form_and_upload": {
                const formReadback = await fill(action.fields);
                const attachmentFileName = await upload(action, "upload_element");
                const captured = await snapshot(96);
                captured.elements.push({
                    element_id: crypto.randomUUID(),
                    role: "generic",
                    accessible_name: attachmentFileName,
                    value: null,
                    element_revision: captured.page.document_revision
                });
                return { form_readback: formReadback, attachment_file_name: attachmentFileName, snapshot: captured };
            }
            case "activate_element":
            {
                const element = resolveElement(action.element);
                if (action.activation_class?.kind === "send_external") {
                    throw new Error("exact_send_not_available");
                }
                if (isReviewedSendControl(element)) {
                    throw new Error("send_control_not_available");
                }
                element.click();
                await new Promise((resolve) => setTimeout(resolve, 200));
                return { activated: true, snapshot: await snapshot(64) };
            }
            case "wait_for": {
                const deadline = Date.now() + action.timeout_ms;
                while (Date.now() < deadline) {
                    const element = elements.get(action.element.element_id);
                    const present = Boolean(element?.isConnected);
                    const enabled = present && !element.disabled && element.getAttribute("aria-disabled") !== "true";
                    if (
                        (action.state === "present" && present) ||
                        (action.state === "absent" && !present) ||
                        (action.state === "enabled" && enabled) ||
                        (action.state === "disabled" && present && !enabled)
                    ) {
                        return { matched: true, snapshot: await snapshot(64) };
                    }
                    await new Promise((resolve) => setTimeout(resolve, 100));
                }
                throw new Error("wait_timeout");
            }
            default:
                throw new Error("unsupported_action");
        }
    }

    chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
        if (message?.type !== "lcxl_browser_action") {
            return false;
        }
        void handle(message.action)
            .then((result) => sendResponse({ ok: true, result }))
            .catch((error) => sendResponse({ ok: false, error_code: error instanceof Error ? error.message : "content_script_error" }));
        return true;
    });
})();
