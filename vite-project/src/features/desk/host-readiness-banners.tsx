import React from "react"
import { useTranslation } from "react-i18next"

import { Button } from "@/components/ui/button"
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert"
import { useToast } from "@/hooks/use-toast"
import { deskErrorKeyOr, type ErrorCodeKeyMap } from "@/lib/desk-error-i18n"
import { deskErrorCodeEnum } from "@/services/types"
import { useQueryServerInfo } from "@/services/hooks/systemController/useQueryServerInfo"
import { useAuthorizeWayland } from "@/services/hooks/hostReadinessController/useAuthorizeWayland"
import { useCancelWayland } from "@/services/hooks/hostReadinessController/useCancelWayland"
import { useRequestMacosPermissions } from "@/services/hooks/hostReadinessController/useRequestMacosPermissions"
import { ServiceInstallDialog } from "@/features/layout/service-install-dialog"

const WAYLAND_PORTAL_REASON_KEYS: ErrorCodeKeyMap = {
    [deskErrorCodeEnum.WAYLAND_PORTAL_AUTHORIZATION_REQUIRED]:
        "pages.hostReadiness.wayland.authorizationRequired",
    [deskErrorCodeEnum.WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED]:
        "pages.hostReadiness.wayland.inputPermissionRequired",
    [deskErrorCodeEnum.WAYLAND_PORTAL_AUTHORIZATION_CANCELLED]:
        "pages.hostReadiness.wayland.authorizationCancelled",
    [deskErrorCodeEnum.WAYLAND_PORTAL_SESSION_CLOSED]:
        "pages.hostReadiness.wayland.sessionClosed",
    [deskErrorCodeEnum.WAYLAND_PORTAL_BACKEND_FAILED]:
        "pages.hostReadiness.wayland.backendFailed",
    [deskErrorCodeEnum.FEATURE_UNAVAILABLE]:
        "pages.hostReadiness.wayland.unavailable",
    [deskErrorCodeEnum.PRECONDITION_FAILED]:
        "pages.hostReadiness.wayland.workerUnavailable",
}

export function waylandPortalReasonKey(errorCode: number | null | undefined): string {
    return deskErrorKeyOr(
        WAYLAND_PORTAL_REASON_KEYS,
        errorCode,
        "pages.hostReadiness.wayland.genericFailure",
    )
}

function newOperationId(): string {
    return globalThis.crypto?.randomUUID?.()
        ?? `portal-${Date.now()}-${Math.random().toString(16).slice(2)}`
}

export function isLoopbackHostname(hostname: string): boolean {
    const normalized = hostname.toLowerCase().replace(/^\[|\]$/g, "")
    return normalized === "localhost"
        || normalized === "::1"
        || normalized.startsWith("127.")
}

export function shouldShowWaylandLocalOnlyHint(
    localPermissionOrigin: boolean,
    phase: string,
    inputReady: boolean,
    requiresLocalAction: boolean,
): boolean {
    return !localPermissionOrigin
        && (requiresLocalAction || (phase === "ready" && !inputReady))
}

export function HostReadinessBanners() {
    const { t } = useTranslation()
    const { toast } = useToast()
    const { data: response, refetch } = useQueryServerInfo({
        query: {
            refetchInterval: (query) => {
                const portal = query.state.data?.data?.wayland_portal
                return portal?.phase === "preparing" || portal?.phase === "restoring"
                    ? 2_000
                    : 30_000
            },
        },
    })
    const authorizeWaylandMutation = useAuthorizeWayland()
    const cancelWaylandMutation = useCancelWayland()
    const requestMacosPermissionsMutation = useRequestMacosPermissions()
    const info = response?.data
    const [serviceDialogOpen, setServiceDialogOpen] = React.useState(false)
    const [pendingAction, setPendingAction] = React.useState<string | null>(null)

    const run = async (key: string, action: () => Promise<unknown>) => {
        if (pendingAction) return
        setPendingAction(key)
        try {
            await action()
            await refetch()
        } catch (error) {
            toast({
                variant: "destructive",
                title: t("pages.hostReadiness.actionFailed"),
                description: error instanceof Error ? error.message : String(error),
            })
        } finally {
            setPendingAction(null)
        }
    }

    if (!info) return null

    const portal = info.wayland_portal
    const portalPending = portal?.phase === "preparing" || portal?.phase === "restoring"
    const localPermissionOrigin = isLoopbackHostname(globalThis.location?.hostname ?? "")
    const showWaylandLocalOnlyHint = portal && shouldShowWaylandLocalOnlyHint(
        localPermissionOrigin,
        portal.phase,
        portal.input_ready,
        portal.requires_local_action,
    )
    const authorize = (target: "screen_only" | "screen_and_input") => run(
        `wayland-${target}`,
        () => authorizeWaylandMutation.mutateAsync({
            data: { operation_id: newOperationId(), target },
        }),
    )

    return (
        <div className="space-y-2" data-testid="host-readiness-banners">
            {info.platform === "windows"
                && info.startup_mode === "default"
                && info.service_running && (
                <Alert>
                    <AlertTitle>{t("pages.hostReadiness.serviceRestart.title")}</AlertTitle>
                    <AlertDescription>
                        {t("pages.hostReadiness.serviceRestart.description")}
                    </AlertDescription>
                </Alert>
            )}

            {info.platform === "windows"
                && info.startup_mode === "default"
                && !info.service_installed && (
                <>
                    {!info.server_binary_available ? (
                        <Alert variant="destructive">
                            <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                            <AlertDescription>{t("pages.layout.serviceBanner.binaryNotFound")}</AlertDescription>
                        </Alert>
                    ) : !info.is_admin ? (
                        <Alert variant="destructive">
                            <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                            <AlertDescription>{t("pages.layout.serviceBanner.needsAdmin")}</AlertDescription>
                        </Alert>
                    ) : (
                        <Alert className="flex items-center justify-between gap-4">
                            <div>
                                <AlertTitle>{t("pages.layout.serviceBanner.title")}</AlertTitle>
                                <AlertDescription>{t("pages.layout.serviceBanner.description")}</AlertDescription>
                            </div>
                            <Button size="sm" onClick={() => setServiceDialogOpen(true)}>
                                {t("pages.layout.serviceBanner.installButton")}
                            </Button>
                        </Alert>
                    )}
                    <ServiceInstallDialog
                        open={serviceDialogOpen}
                        onOpenChange={setServiceDialogOpen}
                        defaultInstallPath={info.default_install_path ?? ""}
                    />
                </>
            )}

            {info.macos_permissions
                && (!info.macos_permissions.screen_recording
                    || !info.macos_permissions.accessibility) && (
                <Alert className="flex items-center justify-between gap-4">
                    <div>
                        <AlertTitle>{t("pages.hostReadiness.macos.title")}</AlertTitle>
                        <AlertDescription>
                            <span className="block">
                                {t("pages.hostReadiness.macos.description", {
                                    permissions: [
                                        !info.macos_permissions.screen_recording
                                            ? t("pages.system.settings.macos.permissions.screenRecording")
                                            : null,
                                        !info.macos_permissions.accessibility
                                            ? t("pages.system.settings.macos.permissions.accessibility")
                                            : null,
                                    ].filter(Boolean).join(", "),
                                })}
                            </span>
                            {!localPermissionOrigin && (
                                <span className="mt-1 block">
                                    {t("pages.hostReadiness.localOnly")}
                                </span>
                            )}
                        </AlertDescription>
                    </div>
                    {localPermissionOrigin && (
                        <Button
                            size="sm"
                            disabled={pendingAction !== null}
                            onClick={() => void run(
                                "macos",
                                () => requestMacosPermissionsMutation.mutateAsync(),
                            )}
                        >
                            {t("pages.hostReadiness.macos.action")}
                        </Button>
                    )}
                </Alert>
            )}

            {portal && (
                <Alert variant={portal.phase === "failed" ? "destructive" : "default"}>
                    <div className="flex items-center justify-between gap-4">
                        <div>
                            <AlertTitle>
                                {portal.phase === "ready"
                                    ? t("pages.hostReadiness.wayland.readyTitle")
                                    : t("pages.hostReadiness.wayland.title")}
                            </AlertTitle>
                            <AlertDescription>
                                <span className="block">
                                    {portalPending
                                        ? t("pages.hostReadiness.wayland.pending")
                                        : portal.phase === "ready"
                                            ? t("pages.hostReadiness.wayland.readyDescription", {
                                                persistence: portal.persistent_restore
                                                    ? t("pages.hostReadiness.wayland.persistent")
                                                    : t("pages.hostReadiness.wayland.sessionOnly"),
                                            })
                                            : portal.reason_code != null
                                                ? t(waylandPortalReasonKey(portal.reason_code))
                                                : t("pages.hostReadiness.wayland.description")}
                                </span>
                                {showWaylandLocalOnlyHint ? (
                                    <span className="mt-1 block">
                                        {t("pages.hostReadiness.localOnly")}
                                    </span>
                                ) : portal.phase === "ready" && !portal.input_ready && (
                                    <span className="mt-1 block">
                                        {t("pages.hostReadiness.wayland.enableInputDescription")}
                                    </span>
                                )}
                            </AlertDescription>
                        </div>
                        {localPermissionOrigin && (
                            <div className="flex shrink-0 flex-wrap justify-end gap-2">
                            {portalPending && portal.operation_id ? (
                                <Button
                                    size="sm"
                                    variant="outline"
                                    disabled={pendingAction !== null}
                                    onClick={() => void run(
                                        "wayland-cancel",
                                        () => cancelWaylandMutation.mutateAsync({
                                            data: {
                                                operation_id: portal.operation_id!,
                                                generation: portal.generation,
                                            },
                                        }),
                                    )}
                                >
                                    {t("common.cancel")}
                                </Button>
                            ) : portal.phase === "ready" ? (
                                <>
                                    {!portal.input_ready && (
                                        <Button
                                            size="sm"
                                            disabled={pendingAction !== null}
                                            onClick={() => void authorize("screen_and_input")}
                                        >
                                            {t("pages.hostReadiness.wayland.enableInput")}
                                        </Button>
                                    )}
                                    <Button
                                        size="sm"
                                        variant="outline"
                                        disabled={pendingAction !== null}
                                        onClick={() => void authorize(
                                            portal.input_ready
                                                ? "screen_and_input"
                                                : "screen_only",
                                        )}
                                    >
                                        {t("pages.hostReadiness.wayland.reauthorize")}
                                    </Button>
                                </>
                            ) : portal.phase !== "unsupported" ? (
                                <Button
                                    size="sm"
                                    disabled={pendingAction !== null}
                                    onClick={() => void authorize(portal.recommended_target)}
                                >
                                    {t("pages.hostReadiness.wayland.action")}
                                </Button>
                            ) : null}
                            </div>
                        )}
                    </div>
                </Alert>
            )}
        </div>
    )
}
