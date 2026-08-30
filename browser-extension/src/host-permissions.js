function parsedHttpUrl(value) {
    const parsed = new URL(value);
    const loopback = ["127.0.0.1", "localhost", "[::1]"].includes(parsed.hostname);
    if (parsed.protocol !== "https:" && !(parsed.protocol === "http:" && loopback)) {
        throw new Error("invalid_navigation_target");
    }
    return parsed;
}

function effectivePort(parsed) {
    if (parsed.port) return Number(parsed.port);
    return parsed.protocol === "https:" ? 443 : 80;
}

function browserHostAscii(parsed) {
    const hostname = parsed.hostname.toLowerCase();
    return hostname.startsWith("[") && hostname.endsWith("]")
        ? hostname.slice(1, -1)
        : hostname;
}

export function permissionPatternForUrl(value) {
    const parsed = parsedHttpUrl(value);
    // Chrome match patterns do not bind ports. Exact port and origin matching
    // remain runtime checks in the typed browser protocol and below.
    return `${parsed.protocol}//${parsed.hostname}/*`;
}

export function browserOriginMatchesUrl(origin, value) {
    const parsed = parsedHttpUrl(value);
    const expectedKind = parsed.protocol === "https:" ? "https" : "http_loopback";
    return origin?.kind === expectedKind
        && origin.host_ascii === browserHostAscii(parsed)
        && origin.port === effectivePort(parsed);
}

export async function assertHostPermissionForUrl(chromeApi, value) {
    const pattern = permissionPatternForUrl(value);
    const granted = await chromeApi.permissions.contains({ origins: [pattern] });
    if (!granted) {
        throw new Error("host_permission_revoked");
    }
}

export async function assertTabHostPermission(chromeApi, tabId, expectedOrigin) {
    const tab = await chromeApi.tabs.get(tabId);
    if (!tab?.url || !browserOriginMatchesUrl(expectedOrigin, tab.url)) {
        throw new Error("stale_page_ref");
    }
    await assertHostPermissionForUrl(chromeApi, tab.url);
    return tab;
}
