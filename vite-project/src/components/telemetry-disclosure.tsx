import { useTranslation } from "react-i18next";
import { Info } from "lucide-react";
import {
    Popover,
    PopoverContent,
    PopoverTrigger,
} from "@/components/ui/popover";

export function TelemetryDisclosure() {
    const { t } = useTranslation();

    return (
        <Popover>
            <PopoverTrigger asChild>
                <button
                    type="button"
                    className="inline-flex items-center gap-1 text-primary hover:underline focus:outline-none transition-all"
                >
                    <Info className="h-3 w-3" />
                    <span className="text-xs font-medium">
                        {t("pages.init.telemetry.learnMore")}
                    </span>
                </button>
            </PopoverTrigger>
            <PopoverContent className="w-80 p-4 shadow-xl border-slate-200 dark:border-slate-800 bg-white/95 backdrop-blur-md dark:bg-slate-950/95">
                <div className="space-y-3">
                    <h4 className="font-semibold text-sm leading-none flex items-center gap-2">
                        <Info className="h-4 w-4 text-primary" />
                        {t("pages.init.telemetry.disclosure.title")}
                    </h4>
                    <p className="text-xs text-muted-foreground leading-relaxed">
                        {t("pages.init.telemetry.disclosure.description")}
                    </p>
                    <ul className="text-xs space-y-2 list-disc pl-4 text-slate-700 dark:text-slate-300">
                        <li>{t("pages.init.telemetry.disclosure.item.serviceInfo")}</li>
                        <li>{t("pages.init.telemetry.disclosure.item.osInfo")}</li>
                        <li>{t("pages.init.telemetry.disclosure.item.hostInfo")}</li>
                        <li>{t("pages.init.telemetry.disclosure.item.clientId")}</li>
                    </ul>
                    <div className="pt-2 border-t border-slate-100 dark:border-slate-800">
                        <p className="text-[10px] text-muted-foreground leading-tight italic">
                            {t("pages.init.telemetry.disclosure.footnote")}
                        </p>
                    </div>
                </div>
            </PopoverContent>
        </Popover>
    );
}
