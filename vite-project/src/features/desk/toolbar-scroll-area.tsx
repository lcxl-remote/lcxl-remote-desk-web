import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import type { FocusEventHandler, ReactNode } from 'react';
import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';

export function ToolbarScrollArea({ expanded, vertical, children, onFocus, onBlur }: {
    expanded: boolean;
    vertical: boolean;
    children: ReactNode;
    onFocus?: FocusEventHandler<HTMLDivElement>;
    onBlur?: FocusEventHandler<HTMLDivElement>;
}) {
    const { t } = useTranslation();
    const frame = useRef<HTMLDivElement>(null);
    const viewport = useRef<HTMLDivElement>(null);
    const content = useRef<HTMLDivElement>(null);
    const [scroll, setScroll] = useState({ previous: false, next: false });
    const measure = useCallback(() => {
        const element = viewport.current;
        if (!element) return;
        const position = vertical ? element.scrollTop : element.scrollLeft;
        const length = vertical ? element.clientHeight : element.clientWidth;
        const total = vertical ? element.scrollHeight : element.scrollWidth;
        const previous = expanded && length > 0 && position > 1;
        const next = expanded && length > 0 && position + length < total - 1;
        setScroll(current => current.previous === previous && current.next === next ? current : { previous, next });
    }, [expanded, vertical]);
    useLayoutEffect(() => {
        const element = viewport.current;
        if (!element) return;
        element.scrollLeft = 0;
        element.scrollTop = 0;
    }, [vertical]);
    useLayoutEffect(() => {
        const element = viewport.current, root = frame.current;
        if (!element || !root) return;
        measure();
        const observer = new ResizeObserver(measure);
        observer.observe(element);
        if (content.current) observer.observe(content.current);
        const wheel = (event: WheelEvent) => {
            if (!expanded || event.ctrlKey) return;
            const length = vertical ? element.clientHeight : element.clientWidth;
            const maximum = (vertical ? element.scrollHeight : element.scrollWidth) - length;
            if (length <= 0 || maximum <= 1) return;
            const delta = Math.abs(event.deltaX) > Math.abs(event.deltaY) ? event.deltaX : event.deltaY;
            if (!delta) return;
            const unit = event.deltaMode === 1 ? parseFloat(getComputedStyle(element).lineHeight) || 16
                : event.deltaMode === 2 ? length : 1;
            const position = vertical ? element.scrollTop : element.scrollLeft;
            const next = Math.max(0, Math.min(maximum, position + delta * unit));
            event.preventDefault();
            event.stopPropagation();
            if (vertical) element.scrollTop = next;
            else element.scrollLeft = next;
            measure();
        };
        root.addEventListener('wheel', wheel, { passive: false });
        return () => { observer.disconnect(); root.removeEventListener('wheel', wheel); };
    }, [expanded, measure, vertical]);
    const move = (direction: number) => {
        const element = viewport.current;
        if (!element) return;
        const distance = direction * Math.max(40, (vertical ? element.clientHeight : element.clientWidth) * .75);
        const behavior = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth';
        element.scrollBy(vertical ? { top: distance, behavior } : { left: distance, behavior });
    };
    const Previous = vertical ? ChevronUp : ChevronLeft;
    const Next = vertical ? ChevronDown : ChevronRight;
    return <div ref={frame} className={`controlBarContent ${expanded ? 'expanded' : 'collapsed'}`}
        data-axis={vertical ? 'vertical' : 'horizontal'} inert={!expanded ? true : undefined}
        onFocus={onFocus} onBlur={onBlur}>
        <div ref={viewport} className="controlBarScrollViewport" onScroll={measure}>
            <div ref={content} className="controlButtons">{children}</div>
        </div>
        {scroll.previous && <Button type="button" variant="unstyled" className="toolbarScrollArrow previous"
            aria-label={t(`pages.desk.toolbarLayout.${vertical ? 'scrollUp' : 'scrollLeft'}`)}
            title={t(`pages.desk.toolbarLayout.${vertical ? 'scrollUp' : 'scrollLeft'}`)} onClick={() => move(-1)}>
            <Previous aria-hidden="true" size={16} />
        </Button>}
        {scroll.next && <Button type="button" variant="unstyled" className="toolbarScrollArrow next"
            aria-label={t(`pages.desk.toolbarLayout.${vertical ? 'scrollDown' : 'scrollRight'}`)}
            title={t(`pages.desk.toolbarLayout.${vertical ? 'scrollDown' : 'scrollRight'}`)} onClick={() => move(1)}>
            <Next aria-hidden="true" size={16} />
        </Button>}
    </div>;
}
