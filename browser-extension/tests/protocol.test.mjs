import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { parseHostCommand } from "../src/protocol.js";

function openCommand() {
    return {
        schema_version: 1,
        type: "request",
        request_id: "request-1",
        action: {
            action: "open_page",
            target: {
                url: "https://mail.google.com/mail/u/0/",
                origin: { kind: "https", host_ascii: "mail.google.com", port: 443 }
            }
        }
    };
}

test("accepts the closed typed browser command", () => {
    assert.deepEqual(parseHostCommand(JSON.stringify(openCommand())), openCommand());
});

test("rejects arbitrary script and unknown fields", () => {
    const command = openCommand();
    command.action.evaluate_script = "document.cookie";
    assert.throws(() => parseHostCommand(command), /unsupported_action/u);
});

test("rejects credentials and a mismatched origin", () => {
    const command = openCommand();
    command.action.target.url = "https://user:secret@mail.google.com/mail";
    assert.throws(() => parseHostCommand(command), /invalid_navigation_target/u);
    command.action.target.url = "https://mail.google.com/mail";
    command.action.target.origin.host_ascii = "example.com";
    assert.throws(() => parseHostCommand(command), /origin_mismatch/u);
});

test("accepts only closed activation classes", () => {
    const command = {
        schema_version: 1,
        type: "request",
        request_id: "request-2",
        action: {
            action: "activate_element",
            page: {
                page_id: "tab-1",
                page_incarnation: "document-1",
                document_revision: 2
            },
            element: {
                page_id: "tab-1",
                page_incarnation: "document-1",
                document_revision: 2,
                element_id: "button-1"
            },
            activation_class: { kind: "write_external_draft" }
        }
    };
    assert.deepEqual(parseHostCommand(command), command);
    command.action.activation_class = { kind: "send_external", payload_sha256: "bad" };
    assert.throws(() => parseHostCommand(command), /invalid_activation_class/u);
    command.action.activation_class = { kind: "write_external_draft", script: "click()" };
    assert.throws(() => parseHostCommand(command), /invalid_activation_class/u);
});

test("manifest keeps tab access user-scoped and excludes privileged browser surfaces", async () => {
    const manifest = JSON.parse(await readFile(new URL("../manifest.json", import.meta.url), "utf8"));
    assert.deepEqual([...manifest.permissions].sort(), ["activeTab", "alarms", "scripting", "storage"]);
    for (const forbidden of ["tabs", "debugger", "cookies", "history", "webRequest", "downloads"]) {
        assert.equal(manifest.permissions.includes(forbidden), false);
    }
    assert.equal(manifest.host_permissions.includes("<all_urls>"), false);
    assert.deepEqual(manifest.optional_host_permissions, ["https://*/*"]);
});

test("MV3 reconnect uses an alarm that survives service-worker suspension", async () => {
    const worker = await readFile(new URL("../src/service-worker.js", import.meta.url), "utf8");
    assert.match(worker, /chrome\.alarms\.create\(RECONNECT_ALARM/u);
    assert.match(worker, /chrome\.alarms\.onAlarm\.addListener/u);
    assert.match(worker, /await chrome\.alarms\.clear\(RECONNECT_ALARM\)/u);
});
