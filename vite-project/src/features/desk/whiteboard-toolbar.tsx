import type { WhiteboardTool } from './use-desk-whiteboard';

type WhiteboardToolbarProps = {
    tool: WhiteboardTool;
    setTool: (t: WhiteboardTool) => void;
    color: string;
    setColor: (c: string) => void;
    strokeWidth: number;
    setStrokeWidth: (w: number) => void;
    onClear: () => void;
    onUndo: () => void;
    onClose: () => void;
};

const COLORS = ['#ff0000', '#00ff00', '#0000ff', '#ffff00', '#ff00ff', '#ffffff', '#000000'];

export default function WhiteboardToolbar({
    tool, setTool, color, setColor, strokeWidth, setStrokeWidth,
    onClear, onUndo, onClose,
}: WhiteboardToolbarProps) {
    return (
        <div style={{
            position: 'absolute', top: 12, left: '50%', transform: 'translateX(-50%)',
            zIndex: 20, display: 'flex', alignItems: 'center', gap: 8,
            background: 'rgba(30, 30, 30, 0.85)', backdropFilter: 'blur(12px)',
            borderRadius: 12, padding: '6px 14px',
            boxShadow: '0 4px 24px rgba(0,0,0,0.4)', color: '#fff', fontSize: 13,
        }}>
            {/* Tool buttons */}
            <button
                onClick={() => setTool('pen')}
                style={toolBtn(tool === 'pen')}
                title="Pen"
            >
                ✏️
            </button>
            <button
                onClick={() => setTool('text')}
                style={toolBtn(tool === 'text')}
                title="Text"
            >
                T
            </button>

            <div style={{ width: 1, height: 24, background: 'rgba(255,255,255,0.2)' }} />

            {/* Color picker */}
            {COLORS.map(c => (
                <button
                    key={c}
                    onClick={() => setColor(c)}
                    style={{
                        width: 20, height: 20, borderRadius: '50%',
                        background: c, border: color === c ? '2px solid #fff' : '2px solid transparent',
                        cursor: 'pointer', padding: 0, minWidth: 0,
                    }}
                />
            ))}

            <div style={{ width: 1, height: 24, background: 'rgba(255,255,255,0.2)' }} />

            {/* Stroke width */}
            <label style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                <span style={{ fontSize: 11, opacity: 0.7 }}>Size</span>
                <input
                    type="range" min={1} max={12} value={strokeWidth}
                    onChange={e => setStrokeWidth(Number(e.target.value))}
                    style={{ width: 60 }}
                />
            </label>

            <div style={{ width: 1, height: 24, background: 'rgba(255,255,255,0.2)' }} />

            {/* Actions */}
            <button onClick={onUndo} style={actionBtn} title="Undo">↩</button>
            <button onClick={onClear} style={actionBtn} title="Clear All">🗑️</button>
            <button onClick={onClose} style={{ ...actionBtn, color: '#ff6b6b' }} title="Close Whiteboard">✕</button>
        </div>
    );
}

function toolBtn(active: boolean): React.CSSProperties {
    return {
        background: active ? 'rgba(255,255,255,0.25)' : 'transparent',
        border: 'none', color: '#fff', cursor: 'pointer',
        borderRadius: 6, padding: '4px 8px', fontSize: 16,
        fontWeight: active ? 700 : 400,
    };
}

const actionBtn: React.CSSProperties = {
    background: 'transparent', border: 'none', color: '#fff',
    cursor: 'pointer', borderRadius: 6, padding: '4px 8px', fontSize: 16,
};
