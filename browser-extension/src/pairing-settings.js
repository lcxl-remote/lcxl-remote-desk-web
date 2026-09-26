// Address validation is shared by the popup and the background connection owner.
export function parsePairingSettings(settings) {
    if (typeof settings?.bridgeUrl !== "string" || typeof settings?.pairingToken !== "string") return null;
    const bridgeUrl = settings.bridgeUrl.trim();
    const pairingToken = settings.pairingToken.trim();
    if (bridgeUrl.length > 256 || !/^[\x21-\x7e]{16,256}$/.test(pairingToken)) return null;
    let endpoint;
    try { endpoint = new URL(bridgeUrl); } catch { return null; }
    if (endpoint.protocol !== "ws:" || endpoint.hostname !== "127.0.0.1" ||
        endpoint.pathname !== "/browser-extension/v2" || endpoint.username || endpoint.password ||
        endpoint.search || endpoint.hash || endpoint.port === "0") return null;
    // Reject URL parser aliases such as integer/short IPv4 addresses.
    if (bridgeUrl !== endpoint.href) return null;
    return { bridgeUrl, pairingToken };
}
