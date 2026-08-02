import * as React from "react"
import { Loader2 } from "lucide-react"

import { Button, type ButtonProps } from "@/components/ui/button"

export interface AsyncButtonProps extends Omit<ButtonProps, "asChild"> {
    pending: boolean
    pendingLabel?: React.ReactNode
    asChild?: never
}

/** A button that keeps pending presentation and accessibility semantics aligned. */
export const AsyncButton = React.forwardRef<HTMLButtonElement, AsyncButtonProps>(
    (
        {
            pending,
            pendingLabel,
            disabled,
            children,
            "aria-busy": ariaBusy,
            ...props
        },
        ref,
    ) => (
        <Button
            ref={ref}
            disabled={disabled || pending}
            aria-busy={pending || ariaBusy || undefined}
            {...props}
        >
            {pending && <Loader2 className="animate-spin" aria-hidden="true" />}
            {pending && pendingLabel !== undefined ? pendingLabel : children}
        </Button>
    ),
)

AsyncButton.displayName = "AsyncButton"
