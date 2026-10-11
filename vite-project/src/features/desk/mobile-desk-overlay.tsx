import type { MobileInteraction } from './mobile-interaction';

export function MobileDeskOverlay({ model }: { model: MobileInteraction }) {
    const cursor = model.cursor && model.project(model.cursor);
    return <div className="mobileDeskOverlay" aria-hidden="true">
        {model.markers.map(marker => {
            const p = model.project(marker.target);
            return <svg key={marker.id} className={`mobileClickMarker ${marker.kind}`} width="40" height="40"
                style={{ left: p.x - 20, top: p.y - 20 }} viewBox="0 0 40 40">
                <circle cx="20" cy="20" r="15" />
                {(marker.kind === 'double' || marker.kind === 'right') && <circle cx="20" cy="20" r="9" />}
                <circle cx="20" cy="20" r="2" />
            </svg>;
        })}
        {model.mode === 'touchpad' && model.canControl && model.cursorVisible && cursor &&
            <svg className="mobileLocalCursor" width="28" height="34" viewBox="0 0 28 34"
                style={{ left: cursor.x, top: cursor.y }}>
                <path d="M1 1L1 25L7 19L12 31L17 29L12 17L22 17Z" fill="white" stroke="black" strokeWidth="2" />
            </svg>}
    </div>;
}
