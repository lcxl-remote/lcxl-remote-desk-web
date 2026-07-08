import { useTranslation } from "react-i18next";

import {
    Dialog,
    DialogClose,
    DialogContent,
    DialogDescription,
    DialogHeader,
    DialogTitle,
    DialogTrigger,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { cn } from "@/lib/utils";

type LegalDoc = "terms" | "privacy";

// Fixed section ids per document. Each id resolves to
// `pages.legal.<doc>.<id>.heading` / `.body` in the locale files; keep this list
// in sync with the keys defined there.
const DOC_SECTIONS: Record<LegalDoc, readonly string[]> = {
    terms: ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9"],
    privacy: ["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8"],
};

/**
 * An inline link that opens the full text of a legal document (Terms of Service
 * or Privacy Policy) in a modal. The trigger label is the document name so it
 * reads naturally inside the consent sentence.
 */
function LegalDocDialog({ doc, label }: { doc: LegalDoc; label: string }) {
    const { t } = useTranslation();

    return (
        <Dialog>
            <DialogTrigger asChild>
                <button
                    type="button"
                    className="text-primary font-medium hover:underline focus:outline-none focus-visible:underline"
                >
                    {label}
                </button>
            </DialogTrigger>
            <DialogContent className="max-w-2xl">
                <DialogHeader>
                    <DialogTitle>{t(`pages.legal.${doc}.title`)}</DialogTitle>
                    <DialogDescription>{t("pages.legal.dialog.lastUpdated")}</DialogDescription>
                </DialogHeader>
                <div className="max-h-[60vh] space-y-4 overflow-y-auto pr-2 text-sm leading-relaxed">
                    <p className="text-muted-foreground">{t(`pages.legal.${doc}.intro`)}</p>
                    {DOC_SECTIONS[doc].map((id) => (
                        <section key={id} className="space-y-1">
                            <h4 className="font-semibold">{t(`pages.legal.${doc}.${id}.heading`)}</h4>
                            <p className="text-muted-foreground">{t(`pages.legal.${doc}.${id}.body`)}</p>
                        </section>
                    ))}
                    <p className="border-t pt-3 text-xs italic text-muted-foreground">
                        {t("pages.legal.dialog.operatorNotice")}
                    </p>
                </div>
                <DialogClose asChild>
                    <Button type="button" variant="secondary" className="w-full">
                        {t("pages.legal.dialog.close")}
                    </Button>
                </DialogClose>
            </DialogContent>
        </Dialog>
    );
}

/**
 * A required-consent control: a checkbox plus a sentence whose "Terms of
 * Service" and "Privacy Policy" fragments open the respective documents. The
 * caller owns the checked state and gates submission on it; only the leading
 * text is bound to the checkbox label so clicking a document link opens the
 * dialog without toggling the checkbox.
 */
export function AgreementConsent({
    checked,
    onCheckedChange,
    id = "agreement-consent",
    className,
}: {
    checked: boolean;
    onCheckedChange: (checked: boolean) => void;
    id?: string;
    className?: string;
}) {
    const { t } = useTranslation();

    return (
        <div className={cn("flex flex-row items-start gap-2", className)}>
            <Checkbox
                id={id}
                checked={checked}
                onCheckedChange={(value) => onCheckedChange(value === true)}
                className="mt-0.5"
            />
            <div className="text-sm leading-relaxed text-muted-foreground">
                <label htmlFor={id} className="cursor-pointer">
                    {t("pages.legal.consent.prefix")}
                </label>{" "}
                <LegalDocDialog doc="terms" label={t("pages.legal.consent.terms")} />{" "}
                {t("pages.legal.consent.and")}{" "}
                <LegalDocDialog doc="privacy" label={t("pages.legal.consent.privacy")} />
            </div>
        </div>
    );
}
