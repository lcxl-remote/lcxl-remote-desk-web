import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import vm from "node:vm";

const source = await readFile(new URL("../src/content-script.js", import.meta.url), "utf8");

// Exercise the unmodified content-script message entry, snapshot refs and click.
// This DOM fixture is a receipt regression test, not evidence of external delivery.
async function send(site, existing, onClick, accountOptions = {}) {
    class Element {
        constructor(tag, name, text = "") {
            this.tagName = tag;
            this.name = name;
            this.textContent = text;
            this.value = text;
            this.isConnected = true;
            this.hidden = false;
        }
        getAttribute(name) { return name === "aria-label" ? this.name : name === "href" ? this.href : name === "data-member-id" ? this.member : name === "data-team-id" ? this.team : null; }
        getBoundingClientRect() { return { width: 100, height: 20 }; }
        closest() { return null; }
    }
    class Input extends Element {}
    const body = new Input("INPUT", "Message", "Daily status");
    const button = new Element("BUTTON", "Send");
    const notices = existing.map(({ hidden = false, text }) => Object.assign(new Element("DIV", "", text), { hidden }));
    const account = Object.assign(new Element("A", "Google Account: Owner (owner@example.test)"), {
        href: "https://accounts.google.com/SignOutOptions?hl=en", member: "U456", team: "T123"
    });
    const accounts = accountOptions.absent ? [] : [account];
    if (accountOptions.ambiguous) accounts.push(account);
    if (accountOptions.initialLabel !== undefined) account.name = accountOptions.initialLabel;
    if (accountOptions.initialHref !== undefined) account.href = accountOptions.initialHref;
    if (accountOptions.hidden) account.hidden = true;
    if (accountOptions.member !== undefined) account.member = accountOptions.member;
    if (accountOptions.team !== undefined) account.team = accountOptions.team;
    let clicks = 0;
    button.click = () => { clicks++; onClick?.(notices, Element, body, account); };
    let listener;
    let now = 0;
    const context = vm.createContext({
        crypto: webcrypto, TextEncoder, TextDecoder, URL,
        HTMLInputElement: Input, HTMLTextAreaElement: class {}, HTMLSelectElement: class {},
        location: new URL(site === "gmail_web" ? "https://mail.google.com/mail/u/0/" : "https://app.slack.com/client/T123/C789"),
        document: { querySelectorAll: selector => (selector === "a[href]" || selector.startsWith('[data-qa="user-button"]')) ? accounts : selector.startsWith("button,") ? [button, body] : notices },
        getComputedStyle: element => ({ visibility: element.hidden ? "hidden" : "visible", display: "block" }),
        Date: class extends Date { static now() { return now; } },
        setTimeout: callback => { now += 100; queueMicrotask(callback); },
        chrome: { runtime: { onMessage: { addListener: fn => { listener = fn; } } } }
    });
    vm.runInContext(source, context);
    const request = action => new Promise(resolve => listener({ type: "lcxl_browser_action", action }, {}, resolve));
    const captured = await request({ action: "take_snapshot", max_elements: 64 });
    assert.equal(captured.ok, true);
    const snapshot = captured.result.snapshot;
    if (site === "gmail_web" && Object.keys(accountOptions).length === 0) {
        assert.equal(snapshot.page.account_id, "gmail-web:owner@example.test");
    }
    if (site === "slack_web" && Object.keys(accountOptions).length === 0) {
        assert.equal(snapshot.page.account_id, "slack-web:T123:U456");
    }
    const ref = name => ({ ...snapshot.elements.find(element => element.accessible_name === name),
        page_incarnation: snapshot.page.page_incarnation, document_revision: snapshot.page.document_revision });
    if (accountOptions.afterSnapshot) accountOptions.afterSnapshot(account, accounts);
    const result = await request({ action: "activate_element", element: ref("Send"), activation_class: {
        kind: "send_external", site, fields: [{ element: ref("Message"), value: "Daily status" }],
        attachment_file_names: [], snapshot_id: "reviewed", payload_sha256: "a".repeat(64), idempotency_key: "test-run"
    } });
    assert.equal(result.ok, true);
    assert.equal(clicks, accountOptions.reject ? 0 : 1);
    return result.result.send_receipt;
}

for (const site of ["gmail_web", "slack_web"]) {
    const acknowledgement = site === "gmail_web" ? "Message sent" : "Daily status";
    test(`${site}: old acknowledgement never confirms another send`, async () => {
        const receipt = await send(site, [{ text: acknowledgement }]);
        assert.equal(receipt.outcome, "outcome_unknown");
        assert.equal(receipt.provider_receipt_id, null);
    });
    test(`${site}: revealing an old hidden acknowledgement is not a new receipt`, async () => {
        const receipt = await send(site, [{ text: acknowledgement, hidden: true }], notices => { notices[0].hidden = false; });
        assert.equal(receipt.outcome, "outcome_unknown");
    });
    test(`${site}: hidden new acknowledgement cannot confirm send`, async () => {
        const receipt = await send(site, [], (notices, Element) => {
            notices.push(Object.assign(new Element("DIV", "", acknowledgement), { hidden: true }));
        });
        assert.equal(receipt.outcome, "outcome_unknown");
    });
    test(`${site}: visible new acknowledgement after the actual click confirms send`, async () => {
        const receipt = await send(site, [{ text: acknowledgement }], (notices, Element) => {
            notices.push(new Element("DIV", "", acknowledgement));
        });
        assert.equal(receipt.outcome, "sent");
        assert.equal(receipt.idempotency_key, "test-run");
        assert.ok(receipt.provider_receipt_id);
    });
}

for (const [name, options] of [
    ["missing account control", { absent: true }],
    ["ambiguous account controls", { ambiguous: true }],
    ["hidden account control", { hidden: true }],
    ["missing email", { initialLabel: "Google Account: Owner" }],
    ["ambiguous email", { initialLabel: "owner@example.test other@example.test" }],
    ["untrusted account link", { initialHref: "https://accounts.google.com.evil.test/SignOutOptions" }],
    ["changed account", { afterSnapshot: account => { account.name = "Google Account: Other (other@example.test)"; } }],
    ["removed account", { afterSnapshot: (_, accounts) => { accounts.length = 0; } }],
    ["account appeared only after snapshot", { absent: true, afterSnapshot: (_, accounts) => {
        accounts.push({ isConnected: true, getBoundingClientRect: () => ({ width: 100, height: 20 }),
            getAttribute: key => key === "href" ? "https://accounts.google.com/SignOutOptions" : "owner@example.test" });
    } }],
]) {
    test(`gmail_web: ${name} rejects before click`, async () => {
        const receipt = await send("gmail_web", [], undefined, { ...options, reject: true });
        assert.equal(receipt.outcome, "definitely_not_sent");
        assert.equal(receipt.evidence, "precondition_rejected_before_activation");
        assert.equal(receipt.provider_receipt_id, null);
    });
}

test("gmail_web: account changed after click remains unknown despite a new acknowledgement", async () => {
    const receipt = await send("gmail_web", [], (notices, Element, _, account) => {
        account.name = "Google Account: Other (other@example.test)";
        notices.push(new Element("DIV", "", "Message sent"));
    });
    assert.equal(receipt.outcome, "outcome_unknown");
    assert.equal(receipt.provider_receipt_id, null);
});

test("gmail_web: localized label with the same account preserves the reviewed activation", async () => {
    const receipt = await send("gmail_web", [], (notices, Element) => {
        notices.push(new Element("DIV", "", "Message sent"));
    }, { afterSnapshot: account => { account.name = "Google 账号：用户 (owner@example.test)"; } });
    assert.equal(receipt.outcome, "sent");
});

for (const [name, options] of [
    ["missing account control", { absent: true }],
    ["ambiguous account controls", { ambiguous: true }],
    ["hidden account control", { hidden: true }],
    ["missing member", { member: null }],
    ["display name instead of member", { member: "Owner" }],
    ["another workspace", { team: "T999" }],
    ["changed member", { afterSnapshot: account => { account.member = "U999"; } }],
    ["changed workspace", { afterSnapshot: account => { account.team = "T999"; } }],
    ["removed account", { afterSnapshot: (_, accounts) => { accounts.length = 0; } }],
]) {
    test(`slack_web: ${name} rejects before click`, async () => {
        const receipt = await send("slack_web", [], undefined, { ...options, reject: true });
        assert.equal(receipt.outcome, "definitely_not_sent");
        assert.equal(receipt.evidence, "precondition_rejected_before_activation");
        assert.equal(receipt.provider_receipt_id, null);
    });
}

test("slack_web: account switched after click cannot confirm delivery", async () => {
    const receipt = await send("slack_web", [], (notices, Element, _, account) => {
        account.member = "U999";
        notices.push(new Element("DIV", "", "Daily status"));
    });
    assert.equal(receipt.outcome, "outcome_unknown");
    assert.equal(receipt.provider_receipt_id, null);
});
