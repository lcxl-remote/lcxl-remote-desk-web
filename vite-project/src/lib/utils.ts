import { type ClassValue, clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
    return twMerge(clsx(inputs))
}

/**
 * Portal target for Radix overlays (tooltip / dropdown / popover) that must
 * stay visible during native fullscreen.
 *
 * When an element enters the browser's fullscreen "top layer", only that
 * element's subtree paints above it. Overlays portalled to `document.body`
 * (Radix's default) therefore render *behind* the fullscreen element and become
 * invisible. Returning the current `fullscreenElement` re-parents the overlay
 * inside the top layer; returning `undefined` outside fullscreen lets Radix fall
 * back to its default (`document.body`), preserving existing behaviour.
 */
export function fullscreenPortalContainer(): HTMLElement | undefined {
    if (typeof document === "undefined") return undefined
    return (document.fullscreenElement as HTMLElement | null) ?? undefined
}

export function formatBytes(bytes: number, decimals = 2) {
    if (!+bytes) return '0 Bytes'

    const k = 1024
    const dm = decimals < 0 ? 0 : decimals
    const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB']

    const i = Math.floor(Math.log(bytes) / Math.log(k))

    return `${parseFloat((bytes / Math.pow(k, i)).toFixed(dm))} ${sizes[i]}`
}
