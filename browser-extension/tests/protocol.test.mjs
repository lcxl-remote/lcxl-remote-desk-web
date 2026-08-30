import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { parseHostCommand } from "../src/protocol.js";
import {
    assertHostPermissionForUrl,
    assertTabHostPermission,
    browserOriginMatchesUrl,
    permissionPatternForUrl
} from "../src/host-permissions.js";

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

function extensionChrome(overrides = {}) {
    const event = { addListener() {}, removeListener() {} };
    return {
        alarms: {
            clear: async () => true,
            create() {},
            onAlarm: event
        },
        permissions: {
            contains: async () => true
        },
        runtime: {
            getManifest: () => ({ version: "0.1.0" }),
            onInstalled: event,
            onStartup: event
        },
        scripting: {
            executeScript: async () => undefined
        },
        storage: {
            local: {
                get: async () => ({})
            },
            onChanged: event,
            session: {
                get: async () => ({}),
                set: async () => undefined
            }
        },
        tabs: {
            create: async () => ({ id: 1, status: "complete" }),
            get: async () => ({ id: 1, status: "complete", url: "https://example.com/" }),
            onUpdated: event,
            query: async () => [],
            sendMessage: async () => ({ ok: true, result: {} }),
            update: async () => ({})
        },
        ...overrides
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

test("host permission patterns stay least-privilege while exact ports remain runtime-bound", () => {
    assert.equal(permissionPatternForUrl("https://Example.com:8443/path"), "https://example.com/*");
    assert.equal(
        browserOriginMatchesUrl(
            { kind: "https", host_ascii: "example.com", port: 8443 },
            "https://example.com:8443/path"
        ),
        true
    );
    assert.equal(
        browserOriginMatchesUrl(
            { kind: "https", host_ascii: "example.com", port: 443 },
            "https://example.com:8443/path"
        ),
        false
    );
    assert.equal(permissionPatternForUrl("http://[::1]:8091/path"), "http://[::1]/*");
    assert.equal(
        browserOriginMatchesUrl(
            { kind: "http_loopback", host_ascii: "::1", port: 8091 },
            "http://[::1]:8091/path"
        ),
        true
    );
});

test("popup requests the same port-independent least-privilege match pattern", async () => {
    const popup = await readFile(new URL("../src/popup.js", import.meta.url), "utf8");
    const html = await readFile(new URL("../src/popup.html", import.meta.url), "utf8");
    assert.match(popup, /permissionPatternForUrl\(tab\.url\)/u);
    assert.doesNotMatch(popup, /`\$\{url\.origin\}\/\*`/u);
    assert.match(html, /<script type="module" src="popup\.js"><\/script>/u);
});

test("revoked host permission rejects before any browser action is dispatched", async () => {
    let permissionChecks = 0;
    const chromeApi = {
        permissions: {
            contains: async ({ origins }) => {
                permissionChecks += 1;
                assert.deepEqual(origins, ["https://example.com/*"]);
                return false;
            }
        }
    };

    await assert.rejects(
        () => assertHostPermissionForUrl(chromeApi, "https://example.com/report"),
        /host_permission_revoked/u
    );
    assert.equal(permissionChecks, 1);
});

test("service worker consults live permission before opening a target tab", async () => {
    let createdTabs = 0;
    globalThis.chrome = extensionChrome({
        permissions: { contains: async () => false },
        tabs: {
            ...extensionChrome().tabs,
            create: async () => {
                createdTabs += 1;
                return { id: 1, status: "complete" };
            }
        }
    });
    const workerUrl = new URL("../src/service-worker.js?permission-revoke-test", import.meta.url);
    const { execute } = await import(workerUrl);

    await assert.rejects(() => execute(openCommand().action), /host_permission_revoked/u);
    assert.equal(createdTabs, 0);
    await Promise.resolve();
    delete globalThis.chrome;
});

test("open retries only the read-only descriptor on one newly created tab", async () => {
    let createdTabs = 0;
    let injectedScripts = 0;
    let messageAttempts = 0;
    globalThis.chrome = extensionChrome({
        tabs: {
            ...extensionChrome().tabs,
            create: async () => {
                createdTabs += 1;
                return { id: 9, status: "complete" };
            },
            get: async () => ({
                id: 9,
                status: "complete",
                url: "https://mail.google.com/mail/u/0/#inbox"
            }),
            sendMessage: async (_tabId, message) => {
                messageAttempts += 1;
                assert.equal(message.action.action, "describe_page");
                if (messageAttempts <= 2) {
                    throw new Error("Receiving end does not exist");
                }
                return {
                    ok: true,
                    result: {
                        page: {
                            page_id: null,
                            page_incarnation: "gmail-document",
                            origin: { kind: "https", host_ascii: "mail.google.com", port: 443 },
                            document_revision: 1,
                            url_sha256: "c".repeat(64)
                        }
                    }
                };
            }
        },
        scripting: {
            executeScript: async () => {
                injectedScripts += 1;
            }
        }
    });
    const workerUrl = new URL("../src/service-worker.js?descriptor-retry-test", import.meta.url);
    const { execute } = await import(workerUrl);

    const result = await execute(openCommand().action);

    assert.equal(createdTabs, 1);
    assert.equal(injectedScripts, 1);
    assert.equal(messageAttempts, 3);
    assert.equal(result.page.page_id, "tab-9");
    delete globalThis.chrome;
});

test("descriptor retry shares one deadline across message and injection fallback", async () => {
    let messageAttempts = 0;
    globalThis.chrome = extensionChrome({
        tabs: {
            ...extensionChrome().tabs,
            get: async () => ({ id: 12, status: "loading", url: "https://app.slack.com/" }),
            sendMessage: async () => {
                messageAttempts += 1;
                return new Promise(() => {});
            }
        }
    });
    const workerUrl = new URL("../src/service-worker.js?descriptor-deadline-test", import.meta.url);
    const { describeTabWithRetry } = await import(workerUrl);
    const startedAt = Date.now();

    await assert.rejects(
        () => describeTabWithRetry(12, 40, 1),
        /content_script_timeout/u
    );

    assert.ok(Date.now() - startedAt < 150);
    assert.ok(messageAttempts >= 1);
    delete globalThis.chrome;
});

test("navigation settle timeout reuses the tab returned by create without a second lookup", async () => {
    let tabLookups = 0;
    globalThis.chrome = extensionChrome({
        tabs: {
            ...extensionChrome().tabs,
            get: async () => {
                tabLookups += 1;
                return new Promise(() => {});
            }
        }
    });
    const workerUrl = new URL("../src/service-worker.js?navigation-settle-budget-test", import.meta.url);
    const { waitForComplete } = await import(workerUrl);
    const startedAt = Date.now();

    const tabId = await waitForComplete({ id: 14, status: "loading" }, 20);

    assert.equal(tabId, 14);
    assert.equal(tabLookups, 0);
    assert.ok(Date.now() - startedAt < 150);
    delete globalThis.chrome;
});

test("open_page reuses the exact existing target after an unknown prior open", async () => {
    let createdTabs = 0;
    globalThis.chrome = extensionChrome({
        tabs: {
            ...extensionChrome().tabs,
            create: async () => {
                createdTabs += 1;
                return { id: 23, status: "complete" };
            },
            query: async () => [{
                id: 22,
                status: "complete",
                url: "https://mail.google.com/mail/u/0/#inbox"
            }],
            sendMessage: async () => ({
                ok: true,
                result: {
                    page: {
                        page_id: null,
                        page_incarnation: "gmail-document",
                        origin: { kind: "https", host_ascii: "mail.google.com", port: 443 },
                        document_revision: 1,
                        url_sha256: "c".repeat(64)
                    }
                }
            })
        }
    });
    const workerUrl = new URL("../src/service-worker.js?reuse-existing-target-test", import.meta.url);
    const { execute } = await import(workerUrl);

    const result = await execute(openCommand().action);

    assert.equal(createdTabs, 0);
    assert.equal(result.page.page_id, "tab-22");
    delete globalThis.chrome;
});

test("open_page reuses the remembered tab after an exact target redirects", async () => {
    let createdTabs = 0;
    const target = "https://lcxl-remote.slack.com/";
    globalThis.chrome = extensionChrome({
        storage: {
            ...extensionChrome().storage,
            session: {
                get: async () => ({ openedTargetTabs: { [target]: 31 } }),
                set: async () => undefined
            }
        },
        tabs: {
            ...extensionChrome().tabs,
            create: async () => {
                createdTabs += 1;
                return { id: 32, status: "complete" };
            },
            get: async () => ({
                id: 31,
                status: "complete",
                url: "https://app.slack.com/client/workspace/channel"
            }),
            query: async () => [],
            sendMessage: async () => ({
                ok: true,
                result: {
                    page: {
                        page_id: null,
                        page_incarnation: "slack-document",
                        origin: { kind: "https", host_ascii: "app.slack.com", port: 443 },
                        document_revision: 1,
                        url_sha256: "d".repeat(64)
                    }
                }
            })
        }
    });
    const workerUrl = new URL("../src/service-worker.js?reuse-redirected-target-test", import.meta.url);
    const { execute } = await import(workerUrl);
    const action = openCommand().action;
    action.target = {
        url: target,
        origin: { kind: "https", host_ascii: "lcxl-remote.slack.com", port: 443 }
    };

    const result = await execute(action);

    assert.equal(createdTabs, 0);
    assert.equal(result.page.page_id, "tab-31");
    delete globalThis.chrome;
});

test("same-origin user navigation becomes stale before a typed navigate action", async () => {
    let updatedTabs = 0;
    globalThis.chrome = extensionChrome({
        tabs: {
            ...extensionChrome().tabs,
            get: async () => ({ id: 7, status: "complete", url: "https://example.com/new-document" }),
            sendMessage: async (_tabId, message) => {
                assert.equal(message.action.action, "describe_page");
                return {
                    ok: true,
                    result: {
                        page: {
                            page_id: null,
                            page_incarnation: "new-document",
                            origin: { kind: "https", host_ascii: "example.com", port: 443 },
                            document_revision: 1,
                            url_sha256: "b".repeat(64)
                        }
                    }
                };
            },
            update: async () => {
                updatedTabs += 1;
                return {};
            }
        }
    });
    const workerUrl = new URL("../src/service-worker.js?permission-revoke-test", import.meta.url);
    const { execute } = await import(workerUrl);
    const action = {
        action: "navigate_page",
        page: {
            page_id: "tab-7",
            page_incarnation: "old-document",
            origin: { kind: "https", host_ascii: "example.com", port: 443 },
            document_revision: 1,
            url_sha256: "a".repeat(64)
        },
        target: {
            url: "https://example.com/next",
            origin: { kind: "https", host_ascii: "example.com", port: 443 }
        }
    };

    await assert.rejects(() => execute(action), /stale_page_ref/u);
    assert.equal(updatedTabs, 0);
    delete globalThis.chrome;
});

test("an injected tab becomes unusable immediately after host permission revocation", async () => {
    let permissionChecks = 0;
    const chromeApi = {
        tabs: {
            get: async (tabId) => {
                assert.equal(tabId, 7);
                return { id: 7, url: "https://example.com/injected-before-revoke" };
            }
        },
        permissions: {
            contains: async () => {
                permissionChecks += 1;
                return false;
            }
        }
    };

    await assert.rejects(
        () => assertTabHostPermission(
            chromeApi,
            7,
            { kind: "https", host_ascii: "example.com", port: 443 }
        ),
        /host_permission_revoked/u
    );
    assert.equal(permissionChecks, 1);
});

test("a tab navigated by the user fails stale before its new origin permission is queried", async () => {
    let permissionChecks = 0;
    const chromeApi = {
        tabs: {
            get: async () => ({ id: 7, url: "https://attacker.example/" })
        },
        permissions: {
            contains: async () => {
                permissionChecks += 1;
                return true;
            }
        }
    };

    await assert.rejects(
        () => assertTabHostPermission(
            chromeApi,
            7,
            { kind: "https", host_ascii: "example.com", port: 443 }
        ),
        /stale_page_ref/u
    );
    assert.equal(permissionChecks, 0);
});
