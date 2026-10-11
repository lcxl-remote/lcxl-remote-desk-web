import { Hand, Scan, ZoomIn, ZoomOut } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import type { useMobileDeskInteraction } from './use-mobile-desk-interaction';

export function MobileDeskControls({ interaction, onOpenChange }: {
    interaction: ReturnType<typeof useMobileDeskInteraction>;
    onOpenChange: (open: boolean) => void;
}) {
    const { t } = useTranslation();
    const { model: m, change } = interaction;
    const [isTouchController, setIsTouchController] = useState(() =>
        window.matchMedia?.('(pointer: coarse)').matches ?? false);
    useEffect(() => {
        const query = window.matchMedia?.('(pointer: coarse)');
        if (!query) return;
        const update = () => setIsTouchController(query.matches);
        update();
        query.addEventListener('change', update);
        return () => query.removeEventListener('change', update);
    }, []);
    useEffect(() => {
        if (!isTouchController) {
            // Restore the local view when its touch controls are no longer available.
            change(model => model.resetViewport());
            onOpenChange(false);
        }
    }, [isTouchController, change, onOpenChange]);
    if (!isTouchController) return null;
    const label = (key: string) => t(`pages.desk.mobile.${key}`);
    return <>
        <Popover onOpenChange={open => { change(model => model.cancel(true)); onOpenChange(open); }}>
            <PopoverTrigger asChild><Button className="controlButton" variant="ghost" aria-label={label('controls')}><Hand /></Button></PopoverTrigger>
            <PopoverContent className="w-72 space-y-3" side="top">
                <p className="font-medium">{label('controls')}</p>
                <div className="flex gap-2">
                    <Button variant={m.mode === 'direct' ? 'default' : 'outline'} aria-pressed={m.mode === 'direct'} onClick={() => change(model => model.setMode('direct'))}>{label('direct')}</Button>
                    <Button variant={m.mode === 'touchpad' ? 'default' : 'outline'} aria-pressed={m.mode === 'touchpad'} onClick={() => change(model => model.setMode('touchpad'))}>{label('touchpad')}</Button>
                </div>
                <p>{m.zoom.toFixed(1)}×</p>
                {m.mode === 'touchpad' && m.canControl && <>
                    <Button variant={m.dragArmed ? 'default' : 'outline'} aria-pressed={m.dragArmed} disabled={!m.cursorVisible}
                        onClick={() => change(model => model.toggleDrag())}>{label('drag')}</Button>
                    {!m.cursorVisible && <div><p>{label('offscreen')}</p><Button onClick={() => change(model => model.locateCursor())}>{label('locate')}</Button></div>}
                </>}
                {m.zoom > 1 && <div className="flex gap-2">
                    <Button variant={m.panMode === 'pan' ? 'default' : 'outline'} aria-pressed={m.panMode === 'pan'} onClick={() => change(model => model.setPanMode('pan'))}>{label('pan')}</Button>
                    <Button variant={m.panMode === 'scroll' ? 'default' : 'outline'} disabled={!m.canControl} aria-pressed={m.panMode === 'scroll'} onClick={() => change(model => model.setPanMode('scroll'))}>{label('scroll')}</Button>
                </div>}
                <p className="text-sm text-muted-foreground">{label(m.mode === 'direct' ? 'directHelp' : 'touchpadHelp')}</p>
                <p className="text-sm text-muted-foreground">{label('zoomHelp')}</p>
            </PopoverContent>
        </Popover>
        <Button className="controlButton" variant="ghost" aria-label={label('zoomIn')} disabled={!m.valid || m.zoom >= m.maxZoom}
            onClick={() => change(model => model.zoomBy(1.5))}><ZoomIn /></Button>
        <Button className="controlButton" variant="ghost" aria-label={label('zoomOut')} disabled={!m.valid || m.zoom <= 1}
            onClick={() => change(model => model.zoomBy(1 / 1.5))}><ZoomOut /></Button>
        <Button className="controlButton" variant="ghost" aria-label={label('fit')} disabled={!m.valid}
            onClick={() => change(model => model.resetViewport())}><Scan /></Button>
    </>;
}
