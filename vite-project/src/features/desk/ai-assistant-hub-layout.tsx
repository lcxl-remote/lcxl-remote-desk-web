import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { ArrowLeft } from 'lucide-react';
import { Button } from '@/components/ui/button';

export function AiAssistantHubLayout({ children }: { children: ReactNode }) {
    const { t } = useTranslation();

    return <section className="mx-auto w-full max-w-6xl space-y-4 p-6">
        <Button variant="ghost" size="sm" className="gap-2" asChild>
            <Link to="/ai-assistant">
                <ArrowLeft className="h-4 w-4" aria-hidden="true" />
                {t('pages.aiAssistant.backToOverview')}
            </Link>
        </Button>
        {children}
    </section>;
}
