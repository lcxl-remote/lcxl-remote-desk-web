import { SCHEMA_VERSION, parseHostCommand, response } from "./protocol.js";

const DEFAULT_BRIDGE_URL = "ws://127.0.0.1:8091/browser-extension/v1";
const RECONNECT_ALARM = "lcxl-browser-extension-reconnect";
const RECONNECT_MAX_MS = 30000;
const KEEPALIVE_INTERVAL_MS = 20000;
const NAVIGATION_SETTLE_TIMEOUT_MS = 15000;
let socket = null;
let reconnectDelayMs = 1000;
let reconnectTimer = null;
let keepaliveTimer = null;
let connectionGeneration = 0;

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

async function sendToTab(tabId, action) {
    let reply;
    try {
        reply = await chrome.tabs.sendMessage(tabId, { type: "lcxl_browser_action", action });
    } catch (error) {
        const tab = await chrome.tabs.get(tabId);
        if (!tab.url) {
            throw error;
        }
        await chrome.scripting.executeScript({ target: { tabId }, files: ["src/content-script.js"] });
        reply = await chrome.tabs.sendMessage(tabId, { type: "lcxl_browser_action", action });
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

function tabIdFromPage(page) {
    const match = /^tab-(\d+)$/u.exec(page?.page_id || "");
    if (!match) {
        throw new Error("stale_page_ref");
    }
    return Number(match[1]);
}

async function waitForComplete(tabId) {
    const current = await chrome.tabs.get(tabId);
    if (current.status === "complete") {
        return current;
    }
    return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => {
            chrome.tabs.onUpdated.removeListener(listener);
            // Gmail and Slack are long-lived applications: their tab can stay
            // in `loading` after the actionable DOM and content script are
            // already available. Return the current tab so sendToTab can make
            // a bounded semantic probe. If the document is not actionable,
            // scripting/message delivery still fails with a known error well
            // before the host's 35-second OutcomeUnknown boundary.
            void chrome.tabs.get(tabId).then(resolve, reject);
        }, NAVIGATION_SETTLE_TIMEOUT_MS);
        const listener = (updatedId, info, tab) => {
            if (updatedId === tabId && info.status === "complete") {
                clearTimeout(timeout);
                chrome.tabs.onUpdated.removeListener(listener);
                resolve(tab);
            }
        };
        chrome.tabs.onUpdated.addListener(listener);
    });
}

async function execute(action) {
    if (action.action === "open_page") {
        const tab = await chrome.tabs.create({ url: action.target.url, active: true });
        const complete = await waitForComplete(tab.id);
        return sendToTab(complete.id, { action: "describe_page" });
    }
    const tabId = tabIdFromPage(action.page);
    if (action.action === "navigate_page") {
        await chrome.tabs.update(tabId, { url: action.target.url, active: true });
        await waitForComplete(tabId);
        return sendToTab(tabId, { action: "describe_page" });
    }
    return sendToTab(tabId, action);
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
