import { useTranslation } from "react-i18next";
import { Outlet, useLocation, Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { Button } from "@/components/ui/button";

export function SettingsLayout() {
    const { t } = useTranslation();
    const location = useLocation();

    const isOverview = location.pathname === "/system" || location.pathname === "/system/";

    return (
        <div className="flex flex-col h-full w-full overflow-hidden">
            {!isOverview && (
                <div className="shrink-0 p-4 border-b">
                    <Button variant="ghost" size="sm" className="gap-2" asChild>
                        <Link to="/system">
                            <ArrowLeft className="h-4 w-4" />
                            {t('pages.settings.backToOverview')}
                        </Link>
                    </Button>
                </div>
            )}
            
            <main className="flex-1 overflow-y-auto min-h-0 bg-background rounded-b-lg">
                <Outlet />
            </main>
        </div>
    );
}
