import { SCHEMA_VERSION, parseHostCommand, response } from "./protocol.js";
import { assertHostPermissionForUrl, assertTabHostPermission } from "./host-permissions.js";

const DEFAULT_BRIDGE_URL = "ws://127.0.0.1:8091/browser-extension/v1";
const RECONNECT_ALARM = "lcxl-browser-extension-reconnect";
const RECONNECT_MAX_MS = 30000;
const KEEPALIVE_INTERVAL_MS = 20000;
const NAVIGATION_SETTLE_TIMEOUT_MS = 15000;
const TAB_MESSAGE_TIMEOUT_MS = 4000;
const TAB_DESCRIBE_RETRY_TIMEOUT_MS = 12000;
const TAB_DESCRIBE_RETRY_INTERVAL_MS = 250;
const TAB_QUERY_TIMEOUT_MS = 3000;
const TARGET_TAB_CACHE_KEY = "openedTargetTabs";
const MAX_REMEMBERED_TARGET_TABS = 16;
const SEND_RECEIPTS_KEY = "exactSendReceipts";
const MAX_SEND_RECEIPTS = 128;
let socket = null;
let reconnectDelayMs = 1000;
let reconnectTimer = null;
let keepaliveTimer = null;
let connectionGeneration = 0;
const sendInflight = new Map();

function stopKeepalive() {
    if (keepaliveTimer) {
        clearInterval(keepaliveTimer);
        keepaliveTimer = null;
    }
}

function startKeepalive(activeSocket) {
    stopKeepalive();
    // Chrome may terminate an otherwise idle MV3 service worker after roughly
    // 30 seconds. A message inside that window keeps the authenticated bridge
    // alive without granting any capability or performing a browser action.
    keepaliveTimer = setInterval(() => {
        if (socket === activeSocket && activeSocket.readyState === WebSocket.OPEN) {
            activeSocket.send(JSON.stringify({ schema_version: SCHEMA_VERSION, type: "keepalive" }));
        }
    }, KEEPALIVE_INTERVAL_MS);
}

async function storageGet(area, keys) {
    return chrome.storage[area].get(keys);
}

async function profileIncarnation() {
    const stored = await storageGet("session", ["profileIncarnation"]);
    if (stored.profileIncarnation) {
        return stored.profileIncarnation;
    }
    const value = crypto.randomUUID();
    await chrome.storage.session.set({ profileIncarnation: value });
    return value;
}

function browserVersion() {
    const match = /(?:Chrome|Chromium)\/(\d+(?:\.\d+){0,3})/u.exec(navigator.userAgent);
    return match?.[1] || "unknown";
}

function scheduleReconnect() {
    if (reconnectTimer) {
        return;
    }
    const reconnectAt = Date.now() + reconnectDelayMs;
    // A one-shot alarm survives MV3 service-worker suspension. The timer keeps
    // the fast path at sub-minute latency while the worker is still alive; both
    // converge through connect(), which clears the other trigger.
    chrome.alarms.create(RECONNECT_ALARM, { when: reconnectAt });
    reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        void connect();
    }, reconnectDelayMs);
    reconnectDelayMs = Math.min(reconnectDelayMs * 2, RECONNECT_MAX_MS);
}

function withTimeout(promise, timeoutMs, errorCode) {
    let timeout;
    return Promise.race([
        promise,
        new Promise((_, reject) => {
            timeout = setTimeout(() => reject(new Error(errorCode)), timeoutMs);
        })
    ]).finally(() => clearTimeout(timeout));
}

async function sendTabMessage(tabId, action, timeoutMs = null) {
    const request = chrome.tabs.sendMessage(tabId, { type: "lcxl_browser_action", action });
    return timeoutMs === null
        ? request
        : withTimeout(request, timeoutMs, "content_script_timeout");
}

async function sendToTab(tabId, action, messageTimeoutMs = null) {
    let reply;
    try {
        reply = await sendTabMessage(tabId, action, messageTimeoutMs);
    } catch (error) {
        const tab = await chrome.tabs.get(tabId);
        if (!tab.url) {
            throw error;
        }
        await chrome.scripting.executeScript({ target: { tabId }, files: ["src/content-script.js"] });
        reply = await sendTabMessage(tabId, action, messageTimeoutMs);
    }
    if (!reply?.ok) {
        throw new Error(reply?.error_code || "content_script_error");
    }
    const result = reply.result;
    const bindPageId = (page) => {
        if (page) page.page_id = `tab-${tabId}`;
    };
    bindPageId(result?.page);
    bindPageId(result?.snapshot?.page);
    return result;
}

export async function describeTabWithRetry(
    tabId,
    retryTimeoutMs = TAB_DESCRIBE_RETRY_TIMEOUT_MS,
    retryIntervalMs = TAB_DESCRIBE_RETRY_INTERVAL_MS
) {
    const deadline = Date.now() + retryTimeoutMs;
    let lastError = new Error("content_script_unavailable");
    do {
        try {
            // A Gmail or Slack tab can cross transient documents while its SPA
            // boots. Retrying only this read-only descriptor on the same tab is
            // safe; never repeat tab creation or a mutating browser action. The
            // timeout wraps the complete message -> optional injection -> retry
            // path so one attempt cannot consume the remaining budget twice.
            return await withTimeout(
                sendToTab(tabId, { action: "describe_page" }),
                Math.min(TAB_MESSAGE_TIMEOUT_MS, Math.max(1, deadline - Date.now())),
                "content_script_timeout"
            );
        } catch (error) {
            lastError = error;
        }
        if (Date.now() >= deadline) break;
        await new Promise((resolve) => setTimeout(resolve, retryIntervalMs));
    } while (Date.now() < deadline);
    throw lastError;
}

function tabIdFromPage(page) {
    const match = /^tab-(\d+)$/u.exec(page?.page_id || "");
    if (!match) {
        throw new Error("stale_page_ref");
    }
    return Number(match[1]);
}

export function samePageObservation(expected, current) {
    return expected?.page_id === current?.page_id
        && expected.page_incarnation === current.page_incarnation
        && expected.document_revision === current.document_revision
        && expected.url_sha256 === current.url_sha256
        && expected.origin?.kind === current.origin?.kind
        && expected.origin?.host_ascii === current.origin?.host_ascii
        && expected.origin?.port === current.origin?.port;
}

export async function waitForComplete(tab, settleTimeoutMs = NAVIGATION_SETTLE_TIMEOUT_MS) {
    if (tab.status === "complete") {
        return tab.id;
    }
    return new Promise((resolve) => {
        const timeout = setTimeout(() => {
            chrome.tabs.onUpdated.removeListener(listener);
            // Gmail and Slack are long-lived applications: their tab can stay
            // in `loading` after the actionable DOM and content script are
            // already available. Return the current tab so sendToTab can make
            // a bounded semantic probe. If the document is not actionable,
            // scripting/message delivery still fails with a known error well
            // before the host's 35-second OutcomeUnknown boundary. Do not issue
            // another unbounded chrome.tabs.get() here: the tab identity was
            // already returned by create/update, and only that stable id is
            // needed for the bounded descriptor probe.
            resolve(tab.id);
        }, settleTimeoutMs);
        const listener = (updatedId, info) => {
            if (updatedId === tab.id && info.status === "complete") {
                clearTimeout(timeout);
                chrome.tabs.onUpdated.removeListener(listener);
                resolve(tab.id);
            }
        };
        chrome.tabs.onUpdated.addListener(listener);
    });
}

function navigationUrlWithoutFragment(value) {
    const parsed = new URL(value);
    parsed.hash = "";
    return parsed.href;
}

export async function findExistingTabForTarget(targetUrl, queryTimeoutMs = TAB_QUERY_TIMEOUT_MS) {
    const target = new URL(targetUrl);
    let tabs;
    try {
        // Querying is read-only and bounded. Reusing an already-open exact
        // navigation target avoids duplicate Gmail/Slack tabs after the host
        // conservatively records an earlier open as OutcomeUnknown. Host
        // permission for the exact origin is checked before this function.
        tabs = await withTimeout(
            chrome.tabs.query({ url: `${target.origin}/*` }),
            queryTimeoutMs,
            "tab_query_timeout"
        );
    } catch {
        return null;
    }
    const expected = navigationUrlWithoutFragment(targetUrl);
    return tabs.find((tab) => tab.id !== undefined
        && typeof tab.url === "string"
        && navigationUrlWithoutFragment(tab.url) === expected) || null;
}

async function rememberedTabForTarget(targetUrl, queryTimeoutMs = TAB_QUERY_TIMEOUT_MS) {
    try {
        const stored = await withTimeout(
            storageGet("session", [TARGET_TAB_CACHE_KEY]),
            queryTimeoutMs,
            "target_tab_cache_timeout"
        );
        const tabId = stored?.[TARGET_TAB_CACHE_KEY]?.[targetUrl];
        if (!Number.isInteger(tabId)) return null;
        const tab = await withTimeout(
            chrome.tabs.get(tabId),
            queryTimeoutMs,
            "target_tab_lookup_timeout"
        );
        return tab?.id === tabId ? tab : null;
    } catch {
        return null;
    }
}

async function rememberTargetTab(targetUrl, tabId) {
    try {
        const stored = await withTimeout(
            storageGet("session", [TARGET_TAB_CACHE_KEY]),
            1000,
            "target_tab_cache_timeout"
        );
        const entries = Object.entries(stored?.[TARGET_TAB_CACHE_KEY] || {})
            .filter(([, value]) => Number.isInteger(value) && value !== tabId)
            .slice(-(MAX_REMEMBERED_TARGET_TABS - 1));
        const next = Object.fromEntries(entries);
        next[targetUrl] = tabId;
        await withTimeout(
            chrome.storage.session.set({ [TARGET_TAB_CACHE_KEY]: next }),
            1000,
            "target_tab_cache_timeout"
        );
    } catch {
        // Recovery metadata is best-effort. Failing to remember a created tab
        // must not turn a completed create into an automatic second create.
    }
}

function rawPageFromAction(action) {
    return {
        page_id: action.page.page_id,
        page_incarnation: action.page.page_incarnation,
        origin: action.page.origin,
        document_revision: action.page.document_revision,
        url_sha256: action.page.url_sha256
    };
}

async function cachedSendReceipt(idempotencyKey) {
    const stored = await storageGet("local", [SEND_RECEIPTS_KEY]);
    return stored?.[SEND_RECEIPTS_KEY]?.[idempotencyKey] || null;
}

async function rememberSendReceipt(receipt) {
    const stored = await storageGet("local", [SEND_RECEIPTS_KEY]);
    const entries = Object.entries(stored?.[SEND_RECEIPTS_KEY] || {})
        .filter(([, value]) => value?.idempotency_key !== receipt.idempotency_key)
        .sort((left, right) =>
            Number(left[1]?.observed_at_unix_ms || 0) - Number(right[1]?.observed_at_unix_ms || 0)
        )
        .slice(-(MAX_SEND_RECEIPTS - 1));
    const next = Object.fromEntries(entries);
    next[receipt.idempotency_key] = receipt;
    await chrome.storage.local.set({ [SEND_RECEIPTS_KEY]: next });
}

async function executeOnce(action) {
    if (action.action === "open_page") {
        await assertHostPermissionForUrl(chrome, action.target.url);
        const existing = await findExistingTabForTarget(action.target.url)
            || await rememberedTabForTarget(action.target.url);
        if (existing) {
            return describeTabWithRetry(existing.id);
        }
        const tab = await chrome.tabs.create({ url: action.target.url, active: true });
        await rememberTargetTab(action.target.url, tab.id);
        const tabId = await waitForComplete(tab);
        return describeTabWithRetry(tabId);
    }
    const tabId = tabIdFromPage(action.page);
    await assertTabHostPermission(chrome, tabId, action.page.origin);
    // Every non-open action is authorized against one exact page observation,
    // not merely an origin. Re-read the descriptor immediately before the
    // action so a same-origin navigation between approval and execution fails
    // closed instead of applying the old grant to a different document.
    const current = await sendToTab(tabId, { action: "describe_page" });
    if (!samePageObservation(action.page, current.page)) {
        throw new Error("stale_page_ref");
    }
    if (action.action === "navigate_page") {
        await assertHostPermissionForUrl(chrome, action.target.url);
        const updated = await chrome.tabs.update(tabId, { url: action.target.url, active: true });
        await waitForComplete(updated);
        return describeTabWithRetry(tabId);
    }
    return sendToTab(tabId, action);
}

export async function execute(action) {
    const send = action.action === "activate_element" && action.activation_class?.kind === "send_external";
    if (!send) return executeOnce(action);
    const key = action.activation_class.idempotency_key;
    const cached = await cachedSendReceipt(key);
    if (cached) {
        if (cached.snapshot_id !== action.activation_class.snapshot_id ||
            cached.snapshot_sha256 !== action.activation_class.payload_sha256 ||
            cached.idempotency_key !== key) {
            throw new Error("invalid_cached_send_receipt");
        }
        return { page: rawPageFromAction(action), send_receipt: cached };
    }
    const existing = sendInflight.get(key);
    if (existing) return existing;
    const pending = (async () => {
        const result = await executeOnce(action);
        if (!result?.send_receipt || result.send_receipt.idempotency_key !== key) {
            throw new Error("invalid_send_receipt");
        }
        await rememberSendReceipt(result.send_receipt);
        return result;
    })().finally(() => sendInflight.delete(key));
    sendInflight.set(key, pending);
    return pending;
}

async function handleMessage(event, activeSocket) {
    if (socket !== activeSocket) {
        return;
    }
    let command;
    try {
        const envelope = JSON.parse(event.data);
        if (envelope?.schema_version === SCHEMA_VERSION && envelope?.type === "hello_ack") {
            await chrome.storage.session.set({ connectionState: "connected" });
            startKeepalive(activeSocket);
            return;
        }
        command = parseHostCommand(event.data);
        const result = await execute(command.action);
        if (socket === activeSocket && activeSocket.readyState === WebSocket.OPEN) {
            activeSocket.send(JSON.stringify(response(command.request_id, true, result, null)));
        }
    } catch (error) {
        const requestId = command?.request_id || "invalid";
        if (socket === activeSocket && activeSocket.readyState === WebSocket.OPEN) {
            activeSocket.send(JSON.stringify(response(requestId, false, null, error instanceof Error ? error.message : "extension_error")));
        }
    }
}

async function connect() {
    const settings = await storageGet("local", ["bridgeUrl", "pairingToken"]);
    if (!settings.pairingToken) {
        return;
    }
    const generation = ++connectionGeneration;
    if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
    }
    await chrome.alarms.clear(RECONNECT_ALARM);
    stopKeepalive();
    const bridgeUrl = settings.bridgeUrl || DEFAULT_BRIDGE_URL;
    const previousSocket = socket;
    const activeSocket = new WebSocket(bridgeUrl);
    socket = activeSocket;
    previousSocket?.close();
    void chrome.storage.session.set({ connectionState: "connecting" });
    activeSocket.addEventListener("open", async () => {
        if (socket !== activeSocket || generation !== connectionGeneration) {
            activeSocket.close();
            return;
        }
        reconnectDelayMs = 1000;
        const incarnation = await profileIncarnation();
        if (
            socket !== activeSocket ||
            generation !== connectionGeneration ||
            activeSocket.readyState !== WebSocket.OPEN
        ) {
            return;
        }
        activeSocket.send(JSON.stringify({
            schema_version: SCHEMA_VERSION,
            type: "hello",
            pairing_token: settings.pairingToken,
            extension_version: chrome.runtime.getManifest().version,
            browser_version: browserVersion(),
            profile_incarnation: incarnation
        }));
    });
    activeSocket.addEventListener("message", (event) => void handleMessage(event, activeSocket));
    activeSocket.addEventListener("close", () => {
        if (socket !== activeSocket || generation !== connectionGeneration) {
            return;
        }
        socket = null;
        stopKeepalive();
        void chrome.storage.session.set({ connectionState: "disconnected" });
        scheduleReconnect();
    });
    activeSocket.addEventListener("error", () => activeSocket.close());
}

chrome.runtime.onInstalled.addListener(() => void connect());
chrome.runtime.onStartup.addListener(() => void connect());
chrome.alarms.onAlarm.addListener((alarm) => {
    if (alarm.name === RECONNECT_ALARM) {
        reconnectTimer = null;
        void connect();
    }
});
chrome.storage.onChanged.addListener((_changes, area) => {
    if (area === "local") {
        void connect();
    }
});
void connect();
