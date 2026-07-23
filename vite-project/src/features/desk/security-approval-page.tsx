import { useCallback, useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { ShieldAlert } from "lucide-react";

import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { useQuerySecuritySettings } from "@/services/hooks/securityController/useQuerySecuritySettings";

const ACK_URL = "/api/desk/security-settings/approval/ack";
const SUBMIT_URL = "/api/desk/security-settings/approval/submit";

// The dialog is rendered in its own Tauri window loaded from an external URL,
// so `__TAURI_INTERNALS__` is absent: no Tauri JS API (invoke/listen/window) is
// available. All backend interaction is plain REST (cookie session, same
// origin). The window is closed by the backend (Finish broadcast -> Rust
// `destroy()`), never by the page itself. A user closing the native window is
// mapped to a Deny: the webview never fires `beforeunload` on a native close,
// so Rust calls the exposed `window.__lcxlApprovalDeny` hook from its
// CloseRequested handler (the page only exposes the hook + a browser fallback).
type AckState = "pending" | "ready" | "failed";

export default function SecurityApprovalPage() {
    const { t } = useTranslation();
    const [params] = useSearchParams();
    const reqId = params.get("req_id") ?? "";
    const fromConnectionId = params.get("from_connection_id") ?? "";
    const i18nKey = params.get("i18n_key") ?? "";
    const permissionType = params.get("permission_type") ?? "";

    const [ackState, setAckState] = useState<AckState>("pending");
    const [remember, setRemember] = useState(false);
    const [timeLeft, setTimeLeft] = useState<number | null>(null);
    const [submitting, setSubmitting] = useState(false);
    const [submitError, setSubmitError] = useState(false);

    const submittingRef = useRef(false);
    // True once this window has submitted a result successfully, so the
    // close fallback does not send a spurious deny afterwards.
    const submittedOkRef = useRef(false);
    // True once a deny-on-close has been sent, so the Rust hook and the
    // browser beforeunload fallback never double-submit for the same close.
    const denySentRef = useRef(false);

    const { data: settingsResponse } = useQuerySecuritySettings({
        query: { retry: 3, retryDelay: 1000 },
    });
    const timeout = settingsResponse?.data?.approval_timeout;

    // On mount, tell the backend this dialog is ready. data.ready === true means
    // the request is known and the hub will wait for the user; anything else
    // (ready:false, 401, network error) means "not ready" — buttons stay
    // disabled and the hub's readiness probe denies authoritatively.
    useEffect(() => {
        if (!reqId) {
            setAckState("failed");
            return;
        }
        let cancelled = false;
        (async () => {
            try {
                const res = await fetch(ACK_URL, {
                    method: "POST",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ req_id: reqId }),
                });
                const body = res.ok ? await res.json().catch(() => null) : null;
                if (cancelled) return;
                setAckState(res.ok && body?.data === true ? "ready" : "failed");
            } catch {
                if (!cancelled) setAckState("failed");
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [reqId]);

    // A user closing the native window (X) is a Deny. The external-URL webview
    // does NOT fire `beforeunload` on a native window close, so the Rust side
    // (which owns the window) invokes `window.__lcxlApprovalDeny` from its
    // CloseRequested handler instead. `beforeunload` is kept only as a fallback
    // for a real browser context. The deny is sent regardless of ackState: the
    // hub may be in its readiness-probe phase or already waiting for the user,
    // and a deny resolves either, so the request never deadlocks. Guards:
    // submittedOkRef/submittingRef skip a deny when a real decision is in
    // flight; denySentRef makes the two triggers idempotent. sendBeacon is used
    // so it still works if the page is genuinely unloading.
    useEffect(() => {
        const sendDeny = () => {
            if (
                submittedOkRef.current ||
                submittingRef.current ||
                denySentRef.current ||
                !reqId
            ) {
                return;
            }
            denySentRef.current = true;
            const blob = new Blob(
                [JSON.stringify({ req_id: reqId, approved: false, remember: false })],
                { type: "application/json" },
            );
            navigator.sendBeacon(SUBMIT_URL, blob);
        };
        const w = window as unknown as { __lcxlApprovalDeny?: () => void };
        w.__lcxlApprovalDeny = sendDeny;
        window.addEventListener("beforeunload", sendDeny);
        return () => {
            window.removeEventListener("beforeunload", sendDeny);
            if (w.__lcxlApprovalDeny === sendDeny) delete w.__lcxlApprovalDeny;
        };
    }, [reqId]);

    const submit = useCallback((approved: boolean) => {
        if (ackState !== "ready" || submittingRef.current || submittedOkRef.current) {
            return;
        }
        submittingRef.current = true;
        setSubmitting(true);
        setSubmitError(false);
        (async () => {
            try {
                const res = await fetch(SUBMIT_URL, {
                    method: "POST",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ req_id: reqId, approved, remember }),
                });
                if (res.ok) {
                    submittedOkRef.current = true;
                    setTimeLeft(null);
                    // Backend broadcasts Finished -> Rust destroys this window.
                } else {
                    submittingRef.current = false;
                    setSubmitting(false);
                    setSubmitError(true);
                }
            } catch {
                submittingRef.current = false;
                setSubmitting(false);
                setSubmitError(true);
            }
        })();
    }, [ackState, reqId, remember]);

    // Start the countdown only once the dialog is ready and a positive timeout
    // is configured. approval_timeout null/0 means "wait forever".
    useEffect(() => {
        if (ackState === "ready" && timeout && timeout > 0) {
            setTimeLeft(timeout);
        } else {
            setTimeLeft(null);
        }
    }, [ackState, timeout]);

    useEffect(() => {
        if (timeLeft === null) return;
        if (timeLeft <= 0) {
            submit(false);
            return;
        }
        const timer = setInterval(() => {
            setTimeLeft((prev) => (prev !== null ? prev - 1 : null));
        }, 1000);
        return () => clearInterval(timer);
    }, [timeLeft, submit]);

    const notReady = ackState !== "ready";

    return (
        <div className="flex h-screen w-full items-center justify-center bg-background p-4 select-none">
            <Card className="w-full max-w-md shadow-lg">
                <CardHeader className="space-y-3">
                    <div className="flex items-center gap-3">
                        <ShieldAlert className="h-7 w-7 text-amber-500" />
                        <CardTitle>
                            {t("security.dialog.title")}
                        </CardTitle>
                    </div>
                    <CardDescription>
                        {t("security.dialog.description")}
                        <strong className="text-primary">{fromConnectionId}</strong>
                        {t("security.dialog.description2")}
                    </CardDescription>
                </CardHeader>
                <CardContent className="space-y-4">
                    <div className="rounded-md bg-muted p-4 font-medium text-foreground">
                        {t(i18nKey, permissionType)}
                    </div>

                    {ackState === "pending" && (
                        <p className="text-sm text-muted-foreground">
                            {t("security.dialog.connecting")}
                        </p>
                    )}
                    {ackState === "failed" && (
                        <p className="text-sm text-destructive">
                            {t(
                                "security.dialog.notReady",
                            )}
                        </p>
                    )}
                    {submitError && (
                        <p className="text-sm text-destructive">
                            {t("security.dialog.submitError")}
                        </p>
                    )}

                    <div className="flex items-center space-x-2">
                        <Checkbox
                            id="remember-choice"
                            checked={remember}
                            disabled={notReady || submitting}
                            onCheckedChange={(checked) => setRemember(checked as boolean)}
                        />
                        <label
                            htmlFor="remember-choice"
                            className="cursor-pointer text-sm font-medium leading-none"
                        >
                            {t("security.dialog.rememberChoice")}
                        </label>
                    </div>

                    <div className="flex justify-end gap-2">
                        <Button
                            variant="outline"
                            disabled={notReady || submitting}
                            onClick={() => submit(false)}
                        >
                            {t("security.dialog.deny")}
                            {timeLeft !== null && ` (${timeLeft}s)`}
                        </Button>
                        <Button
                            disabled={notReady || submitting}
                            onClick={() => submit(true)}
                        >
                            {t("security.dialog.allow")}
                        </Button>
                    </div>
                </CardContent>
            </Card>
        </div>
    );
}
