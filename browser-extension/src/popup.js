const bridgeUrl = document.querySelector("#bridge-url");
const pairingToken = document.querySelector("#pairing-token");
const status = document.querySelector("#status");
const connectionState = document.querySelector("#connection-state");

function message(key, substitutions) {
    return chrome.i18n.getMessage(key, substitutions) || key;
}

for (const element of document.querySelectorAll("[data-i18n]")) {
    element.textContent = message(element.dataset.i18n);
}
document.documentElement.lang = chrome.i18n.getUILanguage();

async function refreshConnectionState() {
    const stored = await chrome.storage.session.get(["connectionState"]);
    connectionState.textContent = message(`connection_${stored.connectionState || "disconnected"}`);
}

chrome.storage.onChanged.addListener((_changes, area) => {
    if (area === "session") void refreshConnectionState();
});
void refreshConnectionState();

void chrome.storage.local.get(["bridgeUrl"]).then((stored) => {
    if (stored.bridgeUrl) bridgeUrl.value = stored.bridgeUrl;
});

document.querySelector("#save").addEventListener("click", async () => {
    if (!pairingToken.value.trim()) {
        status.textContent = message("pairingCodeRequired");
        return;
    }
    await chrome.storage.local.set({
        bridgeUrl: bridgeUrl.value.trim(),
        pairingToken: pairingToken.value.trim()
    });
    pairingToken.value = "";
    status.textContent = message("pairingSaved");
});

document.querySelector("#allow-site").addEventListener("click", async () => {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab?.url) {
        status.textContent = message("noActivePage");
        return;
    }
    const url = new URL(tab.url);
    if (url.protocol !== "https:") {
        status.textContent = message("httpsOnly");
        return;
    }
    const originPattern = `${url.origin}/*`;
    const granted = await chrome.permissions.request({ origins: [originPattern] });
    if (!granted) {
        status.textContent = message("siteDenied");
        return;
    }
    await chrome.scripting.executeScript({ target: { tabId: tab.id }, files: ["src/content-script.js"] });
    status.textContent = message("siteAllowed", [url.origin]);
});
