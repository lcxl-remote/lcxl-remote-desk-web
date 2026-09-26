import test from 'node:test';
import assert from 'node:assert/strict';

const flush = () => new Promise(resolve => setImmediate(resolve));
const settings = port => ({ bridgeUrl: `ws://127.0.0.1:${port}/browser-extension/v2`, pairingToken: 'fixture-pairing-secret' });

test('actual worker ignores stale settings, closes invalidated sockets and contains profile failures', async () => {
    const originals = new Map(['chrome', 'WebSocket', 'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval']
        .map(key => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
    const localReads = [];
    const sockets = [];
    const timers = new Map();
    let timerId = 0;
    let changed;
    let rejectProfile = false;
    const passiveEvent = { addListener() {} };
    class Socket {
        static OPEN = 1;
        readyState = 0;
        listeners = new Map();
        constructor(url) { this.url = url; sockets.push(this); }
        addEventListener(name, callback) { this.listeners.set(name, callback); }
        async emit(name, data) { await this.listeners.get(name)?.(data); }
        close() { this.readyState = 3; this.listeners.get('close')?.(); }
        send() { throw new Error('fixture must not send a handshake'); }
    }
    try {
        globalThis.setTimeout = (callback, delay) => { const id = ++timerId; timers.set(id, { callback, delay }); return id; };
        globalThis.clearTimeout = id => timers.delete(id);
        globalThis.setInterval = globalThis.setTimeout;
        globalThis.clearInterval = globalThis.clearTimeout;
        globalThis.WebSocket = Socket;
        globalThis.chrome = {
            runtime: { onInstalled: passiveEvent, onStartup: passiveEvent, onMessage: passiveEvent },
            alarms: { clear: async () => true, create: async () => {}, onAlarm: passiveEvent },
            storage: {
                local: { get: () => new Promise((resolve, reject) => localReads.push({ resolve, reject })) },
                session: {
                    set: async () => {},
                    get: async () => {
                        if (rejectProfile) throw new Error('session storage unavailable');
                        return {};
                    },
                },
                onChanged: { addListener(callback) { changed = callback; } },
            },
        };
        const worker = await import('../src/service-worker.js');
        await flush();
        assert.equal(localReads.length, 1);
        changed({ pairingToken: {} }, 'local');
        await flush();
        assert.equal(localReads.length, 2);
        localReads[1].resolve(settings(12002));
        await flush();
        assert.equal(sockets.length, 1);
        localReads[0].resolve(settings(12001));
        await flush();
        assert.equal(sockets.length, 1, 'older read must not create a socket');
        assert.equal(sockets[0].url, settings(12002).bridgeUrl);

        changed({ openedTargetTabs: {} }, 'local');
        await flush();
        assert.equal(localReads.length, 2, 'page cache writes must not reconnect');
        changed({ bridgeUrl: {} }, 'local');
        assert.equal(sockets[0].readyState, 3, 'old socket closes before awaiting settings');
        await flush();
        localReads[2].resolve({});
        await flush();
        assert.equal(sockets.length, 1, 'invalid settings must stay disconnected');

        changed({ pairingToken: {} }, 'local');
        await flush();
        localReads[3].resolve(settings(12003));
        await flush();
        let sent = 0;
        let injected = 0;
        chrome.tabs = {
            sendMessage: async () => { sent++; throw new Error('response lost after action'); },
            get: async () => ({ url: 'https://example.test/' }),
        };
        chrome.scripting = { executeScript: async () => { injected++; } };
        await assert.rejects(worker.sendToTab(7, { action: 'fill_form' }), /response lost/);
        assert.equal(sent, 1, 'a missing mutation response must never resend the action');
        assert.equal(injected, 0, 'mutation failure must not bootstrap and replay');
        await assert.rejects(worker.sendToTab(7, { action: 'describe_page' }), /response lost/);
        assert.equal(sent, 3, 'read-only bootstrap may retry once');
        assert.equal(injected, 1);
        let allowed = true;
        chrome.permissions = { contains: async () => { allowed = false; return true; } };
        chrome.tabs.query = async () => [];
        let created = 0;
        chrome.tabs.create = async () => { created++; return { id: 8 }; };
        await assert.rejects(worker.execute({ action: 'open_page', target: { url: 'https://example.test/' } },
            () => { if (!allowed) throw new Error('bridge_disconnected'); }), /bridge_disconnected/);
        assert.equal(created, 0, 'connection invalidation during a read must prevent creation');
        rejectProfile = true;
        sockets[1].readyState = Socket.OPEN;
        await sockets[1].emit('open');
        assert.equal(sockets[1].readyState, 3, 'profile failure closes without an unhandled rejection');
        assert.ok([...timers.values()].some(timer => timer.delay === 1000), 'connection failure schedules retry');
    } finally {
        for (const [key, descriptor] of originals) {
            if (descriptor) Object.defineProperty(globalThis, key, descriptor);
            else delete globalThis[key];
        }
    }
});
