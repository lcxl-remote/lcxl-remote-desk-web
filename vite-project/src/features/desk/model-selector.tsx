import { useEffect, useRef, useState } from "react"
import { useTranslation } from "react-i18next"
import { cn } from "@/lib/utils"
import { useToast } from "@/hooks/use-toast"

// Manager-only wire types. The model-selection endpoints (personal
// `GET /api/my/ai/models` / `PUT /api/my/ai/model-preference`, or their org
// counterparts `GET|PUT /api/org/{id}/ai/model{s,-preference}`) and the wallet
// endpoint exist only on the manager, not on the open-source signaling server, so
// they are absent from the shared generated OpenAPI client. This component reaches
// them with a raw `fetch` (same-origin, session-cookie auth) and feature-detects
// their availability: when they are missing (the control end is connected to an
// open-source signal server) the selector renders nothing and the AI flow proceeds
// exactly as before, with no `model_id` attached — that is the API-parity
// guarantee. Which face (personal vs. org) is queried is decided by the optional
// `orgId` prop: absent → personal endpoints (and no `org_id` on the wire); set →
// the org endpoints for that organization.

/** One selectable AI model, mirroring the manager's `AiModelDto`. */
export type AiModelDto = {
    model_id: number
    display_name: string
    role: string
    tier: string
    /** Input-token unit price in micro-points (`value / 1e6 = points`), null when unpriced. */
    input_price_points_micro?: number | null
    /** Output-token unit price in micro-points, null when unpriced. */
    output_price_points_micro?: number | null
    is_default: boolean
    is_current_preference: boolean
}

/** The slice of the manager's `WalletBalanceDto` this component displays. */
type WalletBalanceDto = {
    availableMicro: number
    currency: string
}

/** The manager's `RestResponse<T>` envelope. */
type RestResponse<T> = {
    success: boolean
    code: number
    message?: string | null
    data?: T | null
}

type SaveState = "idle" | "saving" | "error"

/** The AI role slot a selector targets. Diagnose and the terminal copilot both use
 *  the `agent` model; terminal completion is a separate slot. */
export type ModelRole = "agent" | "completion"

type ModelSelectorProps = {
    role: ModelRole
    /**
     * Surfaces the chosen model id to the parent so it rides the next AI request
     * payload's `model_id` field. Called once with the pre-selected model after the
     * catalog loads, then again on each change. Never called while the selector is
     * hidden (open-source signal), so the parent's model id stays null and nothing
     * is sent.
     */
    onChange: (modelId: number | null) => void
    /** Extra classes for the `<select>`, so each host (the dark diagnose overlay vs.
     *  the themed terminal card) can blend it in. */
    className?: string
    /**
     * The active organization id, threaded down from the manager console's org
     * view. When set, the selector queries the org face of the model endpoints
     * (`/api/org/{orgId}/ai/...`) so the catalog, preference, and any `model_id`
     * the parent forwards are all scoped to that org. Left `undefined` by the
     * open-source standalone control end and by the personal view, in which case
     * the personal endpoints are used and nothing org-scoped is sent — preserving
     * the exact behavior of a manager with no org context.
     */
    orgId?: number
    /** Overrides the picker label (defaults to the shared "AI model" string), so a
     *  second selector (e.g. the terminal completion slot) can name its own role. */
    label?: string
}

const MICRO_PER_POINT = 1_000_000

/** Format a micro-point amount as a compact points string (trailing zeros trimmed). */
function pointsFromMicro(micro: number): string {
    const pts = micro / MICRO_PER_POINT
    return pts.toFixed(4).replace(/\.?0+$/, "") || "0"
}

/**
 * A compact AI-model picker for the terminal user. Lists the models the caller may
 * select for `role` (own personal models plus, when enabled, the platform tier),
 * each with a price hint, pre-selecting the current preference (else the default).
 * Persists the choice via `PUT /api/my/ai/model-preference` and surfaces it to the
 * parent for the next request. Renders nothing when the manager endpoints are
 * unavailable (open-source signal) or when no model exists for `role`.
 */
export function ModelSelector({ role, onChange, className, orgId, label }: ModelSelectorProps) {
    const { t } = useTranslation()
    const { toast } = useToast()
    // The manager endpoints to query: the org face when an org is active, else the
    // personal face. `orgId` is a plain number, so it can be interpolated directly.
    const modelsUrl = orgId != null ? `/api/org/${orgId}/ai/models` : "/api/my/ai/models"
    const preferenceUrl =
        orgId != null ? `/api/org/${orgId}/ai/model-preference` : "/api/my/ai/model-preference"
    const [models, setModels] = useState<AiModelDto[] | null>(null)
    const [hidden, setHidden] = useState(false)
    const [selectedId, setSelectedId] = useState<number | null>(null)
    const [balance, setBalance] = useState<{ points: string; currency: string } | null>(null)
    const [saveState, setSaveState] = useState<SaveState>("idle")
    const desiredPersistIdRef = useRef<number | null>(null)
    const persistedIdRef = useRef<number | null>(null)
    const persistInFlightRef = useRef(false)
    const mountedRef = useRef(true)

    // Hold the latest `onChange` in a ref so the load effect need not depend on it
    // (a new closure each render would otherwise re-run the fetch).
    const onChangeRef = useRef(onChange)
    onChangeRef.current = onChange

    useEffect(() => () => {
        mountedRef.current = false
    }, [])

    useEffect(() => {
        let cancelled = false

        // Best-effort wallet balance; a 404 (no manager / no billing) just omits it.
        const loadBalance = async () => {
            try {
                const res = await fetch("/api/billing/my/wallet", {
                    credentials: "include",
                    headers: { Accept: "application/json" },
                })
                if (!res.ok) return
                const body = (await res.json()) as RestResponse<WalletBalanceDto>
                if (cancelled || !body || body.success === false || !body.data) return
                setBalance({
                    points: pointsFromMicro(body.data.availableMicro),
                    currency: body.data.currency,
                })
            } catch {
                // Balance is optional; never block the selector on it.
            }
        }

        const loadModels = async () => {
            try {
                const res = await fetch(modelsUrl, {
                    credentials: "include",
                    headers: { Accept: "application/json" },
                })
                // Non-200 (404 on the open-source signal server) → hide the selector.
                if (!res.ok) {
                    if (!cancelled) setHidden(true)
                    return
                }
                const body = (await res.json()) as RestResponse<AiModelDto[]>
                if (!body || body.success === false || !Array.isArray(body.data)) {
                    if (!cancelled) setHidden(true)
                    return
                }
                const forRole = body.data.filter((m) => m.role === role)
                if (forRole.length === 0) {
                    if (!cancelled) setHidden(true)
                    return
                }
                const pre =
                    forRole.find((m) => m.is_current_preference) ??
                    forRole.find((m) => m.is_default) ??
                    forRole[0]
                if (cancelled) return
                setModels(forRole)
                setSelectedId(pre.model_id)
                desiredPersistIdRef.current = pre.model_id
                persistedIdRef.current = pre.model_id
                setSaveState("idle")
                // Surface the pre-selected model so the first request already carries it.
                onChangeRef.current(pre.model_id)
                // The selector is live; now it is worth fetching the balance.
                void loadBalance()
            } catch {
                if (!cancelled) setHidden(true)
            }
        }

        void loadModels()
        return () => {
            cancelled = true
        }
        // Re-run when the queried face changes (role, or personal↔org via `modelsUrl`).
    }, [role, modelsUrl])

    if (hidden || !models || selectedId === null) return null

    const drainPersistence = async () => {
        if (persistInFlightRef.current) return
        persistInFlightRef.current = true
        if (mountedRef.current) setSaveState("saving")
        try {
            while (
                desiredPersistIdRef.current !== null &&
                desiredPersistIdRef.current !== persistedIdRef.current
            ) {
                const id = desiredPersistIdRef.current
                try {
                    const res = await fetch(preferenceUrl, {
                        method: "PUT",
                        credentials: "include",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ role, model_id: id }),
                    })
                    if (!res.ok) throw new Error("http")
                    const body = (await res.json().catch(() => null)) as RestResponse<unknown> | null
                    if (body && body.success === false) throw new Error("biz")
                    persistedIdRef.current = id
                } catch {
                    if (mountedRef.current) {
                        setSaveState("error")
                        toast({
                            title: t("pages.desk.modelSelector.saveFailed"),
                            variant: "destructive",
                        })
                    }
                    // If the user selected again while this request was pending,
                    // continue with the newest value; otherwise leave it retryable.
                    if (desiredPersistIdRef.current === id) return
                }
            }
            if (mountedRef.current) setSaveState("idle")
        } finally {
            persistInFlightRef.current = false
        }
    }

    const onSelect = (e: React.ChangeEvent<HTMLSelectElement>) => {
        const id = Number(e.target.value)
        if (Number.isNaN(id)) return
        setSelectedId(id)
        onChangeRef.current(id)
        desiredPersistIdRef.current = id
        void drainPersistence()
    }

    /** Compact per-model price hint: input / output unit price in points, or "free". */
    const priceHint = (m: AiModelDto): string => {
        const hasIn = m.input_price_points_micro != null
        const hasOut = m.output_price_points_micro != null
        if (!hasIn && !hasOut) return t("pages.desk.modelSelector.free")
        return t("pages.desk.modelSelector.priceHint", {
            input: hasIn ? pointsFromMicro(m.input_price_points_micro as number) : "—",
            output: hasOut ? pointsFromMicro(m.output_price_points_micro as number) : "—",
        })
    }

    return (
        <div className="flex flex-col gap-1">
            <div className="flex items-center justify-between gap-2">
                <label className="text-xs text-inherit opacity-70">
                    {label ?? t("pages.desk.modelSelector.label")}
                </label>
                {balance && (
                    <span className="text-[10px] text-inherit opacity-60">
                        {t("pages.desk.modelSelector.balance", {
                            points: balance.points,
                        })}
                    </span>
                )}
                {saveState !== "idle" && (
                    <span className="text-[10px] text-inherit opacity-60" aria-live="polite">
                        {t(saveState === "saving"
                            ? "pages.desk.modelSelector.saving"
                            : "pages.desk.modelSelector.saveFailed")}
                    </span>
                )}
            </div>
            <select
                value={selectedId}
                onChange={onSelect}
                className={cn(
                    "w-full rounded-md border px-2 py-1 text-xs outline-none",
                    className,
                )}
            >
                {models.map((m) => (
                    <option key={m.model_id} value={m.model_id} className="bg-neutral-800 text-white">
                        {m.display_name} · {priceHint(m)}
                    </option>
                ))}
            </select>
        </div>
    )
}

export default ModelSelector
