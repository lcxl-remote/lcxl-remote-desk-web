import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronLeft, ChevronRight, Expand, FileText, LoaderCircle, Scan, ZoomIn, ZoomOut } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import type { AiAssistantDocumentPreview } from './ai-assistant-event';

function PreviewImage({ preview, scale }: { preview: AiAssistantDocumentPreview; scale: number | null }) {
    return <img
        src={preview.preview_data_url}
        alt=""
        className={`mx-auto block rounded border bg-white shadow-sm ${scale === null ? 'h-auto max-w-full' : 'max-w-none'}`}
        style={scale === null ? undefined : { width: `${Math.max(25, scale) / 100 * preview.page.width}px` }}
    />;
}

function PreviewCard({ preview, requestPage }: {
    preview: AiAssistantDocumentPreview;
    requestPage: (previewId: string, page: number) => boolean;
}) {
    const { t } = useTranslation();
    const [scale, setScale] = useState<number | null>(null);
    const [expanded, setExpanded] = useState(false);
    const [pageText, setPageText] = useState(String(preview.page.page));
    useEffect(() => setPageText(String(preview.page.page)), [preview.page.page]);
    const navigate = (page: number) => {
        if (requestPage(preview.descriptor.preview_id, page)) setPageText(String(page));
    };
    const submitPage = () => {
        const page = Number(pageText);
        if (!Number.isInteger(page) || page < 1 || page > preview.descriptor.page_count) {
            setPageText(String(preview.page.page));
            return;
        }
        navigate(page);
    };
    return <>
        <Card>
            <CardHeader className="space-y-2 pb-2">
                <div className="flex items-center justify-between gap-2">
                    <CardTitle className="flex items-center gap-2 text-sm">
                        <FileText className="h-4 w-4" />
                        {t('pages.aiAssistant.documentPreview.title')}
                    </CardTitle>
                    <div className="flex items-center gap-1">
                        <Button type="button" size="icon" variant="ghost" className="assistant-detail-icon h-8 w-8"
                            aria-label={t('pages.aiAssistant.documentPreview.zoomOut')}
                            onClick={() => setScale(current => Math.max(25, (current ?? 100) - 25))}>
                            <ZoomOut className="h-4 w-4" />
                        </Button>
                        <span className="min-w-12 text-center text-xs text-muted-foreground">
                            {scale === null ? t('pages.aiAssistant.documentPreview.fitWidth') : `${scale}%`}
                        </span>
                        <Button type="button" size="icon" variant="ghost" className="assistant-detail-icon h-8 w-8"
                            aria-label={t('pages.aiAssistant.documentPreview.zoomIn')}
                            onClick={() => setScale(current => Math.min(200, (current ?? 100) + 25))}>
                            <ZoomIn className="h-4 w-4" />
                        </Button>
                        <Button type="button" size="icon" variant="ghost" className="assistant-detail-icon h-8 w-8"
                            aria-label={t('pages.aiAssistant.documentPreview.fitWidth')}
                            onClick={() => setScale(null)}>
                            <Scan className="h-4 w-4" />
                        </Button>
                        <Button type="button" size="icon" variant="ghost" className="assistant-detail-icon h-8 w-8"
                            aria-label={t('pages.aiAssistant.documentPreview.fullscreen')}
                            onClick={() => setExpanded(true)}>
                            <Expand className="h-4 w-4" />
                        </Button>
                    </div>
                </div>
                <div className="flex items-center gap-2 text-xs text-muted-foreground">
                    <Button type="button" size="icon" variant="outline" className="assistant-detail-icon assistant-detail-icon-outline h-7 w-7"
                        disabled={preview.loading || preview.page.page <= 1}
                        aria-label={t('pages.aiAssistant.documentPreview.previous')}
                        onClick={() => navigate(preview.page.page - 1)}>
                        <ChevronLeft className="h-4 w-4" />
                    </Button>
                    <Input value={pageText} inputMode="numeric" className="h-7 w-16 px-2 text-center text-xs"
                        aria-label={t('pages.aiAssistant.documentPreview.pageNumber')}
                        disabled={preview.loading}
                        onChange={event => setPageText(event.target.value)}
                        onBlur={submitPage}
                        onKeyDown={event => { if (event.key === 'Enter') submitPage(); }} />
                    <span>/ {preview.descriptor.page_count}</span>
                    <Button type="button" size="icon" variant="outline" className="assistant-detail-icon assistant-detail-icon-outline h-7 w-7"
                        disabled={preview.loading || preview.page.page >= preview.descriptor.page_count}
                        aria-label={t('pages.aiAssistant.documentPreview.next')}
                        onClick={() => navigate(preview.page.page + 1)}>
                        <ChevronRight className="h-4 w-4" />
                    </Button>
                    {preview.loading && <LoaderCircle className="h-4 w-4 animate-spin" />}
                </div>
            </CardHeader>
            <CardContent className="space-y-2">
                <div className="max-h-[32rem] overflow-auto rounded-md border bg-muted/30 p-3">
                    <PreviewImage preview={preview} scale={scale} />
                </div>
                {preview.error && <p className="text-xs text-destructive">{preview.error}</p>}
                {preview.descriptor.warnings.map((warning, index) =>
                    <p key={`${warning.code}:${warning.page ?? ''}:${index}`}
                        className="text-xs text-amber-700 dark:text-amber-300">
                        {warning.page ? `${t('pages.aiAssistant.documentPreview.page', { page: warning.page })}: ` : ''}
                        {warning.detail}
                    </p>)}
            </CardContent>
        </Card>
        <Dialog open={expanded} onOpenChange={setExpanded}>
            <DialogContent className="h-[92vh] max-w-[92vw]">
                <DialogHeader><DialogTitle>{t('pages.aiAssistant.documentPreview.title')}</DialogTitle></DialogHeader>
                <div className="min-h-0 overflow-auto bg-muted/30 p-4">
                    <PreviewImage preview={preview} scale={100} />
                </div>
            </DialogContent>
        </Dialog>
    </>;
}

export function AssistantDocumentPreviews({ previews, requestPage }: {
    previews: AiAssistantDocumentPreview[];
    requestPage: (previewId: string, page: number) => boolean;
}) {
    if (previews.length === 0) return null;
    return <div className="space-y-3" data-testid="assistant-document-previews">
        {previews.map(preview => <PreviewCard key={preview.descriptor.preview_id}
            preview={preview} requestPage={requestPage} />)}
    </div>;
}
