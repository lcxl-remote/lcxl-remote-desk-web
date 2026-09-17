import { assertHostPermissionForUrl } from './host-permissions.js';

// HTTP content scripts lack SubtleCrypto. Hashing stays inside the extension.
export function registerContentDigest(chromeApi) {
    chromeApi.runtime.onMessage.addListener((message, sender, sendResponse) => {
        if (message?.type !== 'lcxl_content_digest') return false;
        const digest = async () => {
            if (sender.id !== chromeApi.runtime.id || !Number.isInteger(sender.tab?.id)
                || typeof sender.url !== 'string' || typeof message.base64 !== 'string'
                || message.base64.length > 24 * 1024 * 1024
                || !/^[A-Za-z0-9+/]*={0,2}$/.test(message.base64)) {
                throw new Error('invalid_digest_request');
            }
            await assertHostPermissionForUrl(chromeApi, sender.url);
            const bytes = Uint8Array.from(atob(message.base64), char => char.charCodeAt(0));
            const result = await crypto.subtle.digest('SHA-256', bytes);
            return [...new Uint8Array(result)].map(byte => byte.toString(16).padStart(2, '0')).join('');
        };
        void digest().then(sha256 => sendResponse({ sha256 }), () => sendResponse({ error: 'digest_unavailable' }));
        return true;
    });
}
