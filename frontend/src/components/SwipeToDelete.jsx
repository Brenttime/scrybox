import { useEffect, useRef, useState } from 'react';
import { Trash2 } from 'lucide-react';

// iOS-style swipe-to-delete row. Drag the row left to reveal a red Delete
// action; tap it (or swipe far past it) to delete. Vertical scrolling is left
// alone (touch-action: pan-y) and a drag never counts as a click on the row.
const ACTION = 88; // px width of the revealed action
const FULL = 0.6; // swipe past this fraction of the row width = delete

export default function SwipeToDelete({ onDelete, label, children, className = '' }) {
  const [x, setX] = useState(0);
  const [dragging, setDragging] = useState(false);
  const rowRef = useRef(null);
  const g = useRef(null); // gesture state
  const suppressClick = useRef(false);
  const open = x <= -ACTION;

  // Tapping anywhere else closes an open row.
  useEffect(() => {
    if (!open) return undefined;
    const close = (e) => { if (!rowRef.current?.contains(e.target)) setX(0); };
    document.addEventListener('pointerdown', close, true);
    return () => document.removeEventListener('pointerdown', close, true);
  }, [open]);

  const onPointerDown = (e) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return;
    g.current = { id: e.pointerId, sx: e.clientX, sy: e.clientY, base: x, axis: null };
  };
  const onPointerMove = (e) => {
    const s = g.current;
    if (!s || s.id !== e.pointerId) return;
    const dx = e.clientX - s.sx;
    const dy = e.clientY - s.sy;
    if (!s.axis) {
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
      s.axis = Math.abs(dx) > Math.abs(dy) ? 'x' : 'y';
      if (s.axis === 'x') { setDragging(true); e.currentTarget.setPointerCapture?.(e.pointerId); }
    }
    if (s.axis !== 'x') return;
    const width = rowRef.current?.offsetWidth || 400;
    setX(Math.max(-width, Math.min(0, s.base + dx)));
  };
  const finish = (e) => {
    const s = g.current;
    g.current = null;
    if (!s || s.id !== e.pointerId || s.axis !== 'x') return;
    suppressClick.current = true;
    setDragging(false);
    const width = rowRef.current?.offsetWidth || 400;
    if (-x > width * FULL) { setX(-ACTION); onDelete(() => setX(0)); return; }
    setX(-x > ACTION / 2 ? -ACTION : 0);
  };
  const onClickCapture = (e) => {
    if (suppressClick.current || open) {
      suppressClick.current = false;
      e.stopPropagation();
      e.preventDefault();
      if (open) setX(0);
    }
  };

  return (
    <div ref={rowRef} className={`swipe-row ${className}`}>
      <button type="button" className="swipe-row-action" aria-label={label} title={label}
        tabIndex={open ? 0 : -1} aria-hidden={!open}
        onClick={() => onDelete(() => setX(0))}>
        <Trash2 size={18} />
        <span>{label}</span>
      </button>
      <div className="swipe-row-content"
        style={{ transform: `translateX(${x}px)`, transition: dragging ? 'none' : 'transform 0.25s cubic-bezier(0.16, 1, 0.3, 1)' }}
        onPointerDown={onPointerDown} onPointerMove={onPointerMove}
        onPointerUp={finish} onPointerCancel={finish} onClickCapture={onClickCapture}>
        {children}
      </div>
    </div>
  );
}
