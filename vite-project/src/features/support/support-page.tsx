import { useTranslation } from "react-i18next"

import { SupportCodeCard } from "@/features/support/support-code-card"

// Top-level, host-facing "get remote help" page. It surfaces the temporary
// support code as a primary destination so a non-technical local user can
// request assistance directly, instead of hunting for it inside settings.
export function SupportPage() {
    const { t } = useTranslation()

    return (
        <div className="container mx-auto max-w-4xl py-8">
            <div className="mb-8">
                <h1 className="text-3xl font-bold tracking-tight">{t("pages.support.pageTitle")}</h1>
                <p className="text-muted-foreground">{t("pages.support.pageDescription")}</p>
            </div>

            <SupportCodeCard />
        </div>
    )
}
