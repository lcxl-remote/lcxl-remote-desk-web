import { useState, useEffect } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Checkbox } from "@/components/ui/checkbox";
import type { SecurityApprovalEventPayload } from "@/services/types";
import { useQuerySecuritySettings } from "@/services/hooks/securityController/useQuerySecuritySettings";

export function SecurityApprovalDialog() {
  const { t } = useTranslation();
  const [queue, setQueue] = useState<SecurityApprovalEventPayload[]>([]);
  const [remember, setRemember] = useState(false);
  const [timeLeft, setTimeLeft] = useState<number | null>(null);

  const { data: settingsResponse } = useQuerySecuritySettings();
  const timeout = settingsResponse?.data?.approval_timeout;

  useEffect(() => {
    let unlistenFn: UnlistenFn | undefined;
    let isMounted = true;

    const handleCustomEvent = (e: Event) => {
      const customEvent = e as CustomEvent<SecurityApprovalEventPayload>;
      if (customEvent.detail) {
        setQueue((prev) => {
          if (prev.some(p => p.req_id === customEvent.detail.req_id)) return prev;
          return [...prev, customEvent.detail];
        });
      }
    };

    window.addEventListener("security-approval-request", handleCustomEvent);

    const setupListener = async () => {
      if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
        try {
          const unlisten = await listen<SecurityApprovalEventPayload>(
            "security-approval-request",
            (event) => {
              setQueue((prev) => {
                if (prev.some(p => p.req_id === event.payload.req_id)) return prev;
                return [...prev, event.payload];
              });
            }
          );
          if (isMounted) {
            unlistenFn = unlisten;
          } else {
            unlisten();
          }
        } catch (e) {
          console.error("Failed to setup tauri listener:", e);
        }
      }
    };

    setupListener();

    return () => {
      isMounted = false;
      window.removeEventListener("security-approval-request", handleCustomEvent);
      if (unlistenFn) {
        unlistenFn();
      }
    };
  }, []);

  const currentRequest = queue[0];

  useEffect(() => {
    if (currentRequest && timeout && timeout > 0) {
      setTimeLeft(timeout);
    } else {
      setTimeLeft(null);
    }
  }, [currentRequest, timeout]);

  useEffect(() => {
    if (timeLeft === null) return;

    if (timeLeft <= 0) {
      handleResponse(false);
      return;
    }

    const timer = setInterval(() => {
      setTimeLeft((prev) => (prev !== null ? prev - 1 : null));
    }, 1000);

    return () => clearInterval(timer);
  }, [timeLeft]);

  const handleResponse = async (approved: boolean) => {
    if (!currentRequest) return;
    try {
      await fetch("/api/desk/security-settings/approval/submit", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          req_id: currentRequest.req_id,
          approved,
          remember,
        }),
      });
    } catch (err) {
      console.error("Failed to submit security approval:", err);
    } finally {
      // Reset state and move to next request
      setRemember(false);
      setTimeLeft(null);
      setQueue((prev) => prev.slice(1));
    }
  };

  if (!currentRequest) {
    return null;
  }

  return (
    <AlertDialog open={true}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>
            {t("security.dialog.title", "Security Approval Required")}
          </AlertDialogTitle>
          <AlertDialogDescription>
            {t("security.dialog.description", "A controller with ID ")}
            <strong className="text-primary">{currentRequest.from_connection_id}</strong>
            {t("security.dialog.description2", " is requesting permission to:")}
            <div className="mt-4 mb-4 p-4 bg-muted rounded-md text-foreground font-medium">
              {t(currentRequest.i18n_key, currentRequest.permission_type)}
            </div>
          </AlertDialogDescription>
        </AlertDialogHeader>

        <div className="flex items-center space-x-2 py-2">
          <Checkbox
            id="remember-choice"
            checked={remember}
            onCheckedChange={(checked) => setRemember(checked as boolean)}
          />
          <label
            htmlFor="remember-choice"
            className="text-sm font-medium leading-none cursor-pointer"
          >
            {t("security.dialog.rememberChoice", "Remember my choice")}
          </label>
        </div>

        <AlertDialogFooter>
          <AlertDialogCancel onClick={() => handleResponse(false)}>
            {t("security.dialog.deny", "Deny")}
            {timeLeft !== null && ` (${timeLeft}s)`}
          </AlertDialogCancel>
          <AlertDialogAction onClick={() => handleResponse(true)}>
            {t("security.dialog.allow", "Allow")}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
