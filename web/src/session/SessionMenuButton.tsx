import {useEffect, useRef, useState, type PointerEvent as ReactPointerEvent} from 'react';
import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Icon} from '@astryxdesign/core/Icon';
import {Bars3Icon} from '@heroicons/react/24/outline';
import {readPreference, writePreference} from './storage';

/** Gap kept between the button and the window edge. */
const MARGIN = 8;
/** Pointer travel after which a press is a drag and not a click. */
const DRAG_THRESHOLD = 4;

/**
 * Where the button sits, as fractions of the space it can move in, so that
 * it keeps its place relative to the edges when the window is resized.
 */
type Position = {x: number; y: number};

// The right edge, halfway down: clear of title bars, docks and taskbars.
const DEFAULT_POSITION: Position = {x: 1, y: 0.5};

/** A press in progress: where it started and whether it became a drag. */
type Press = {startX: number; startY: number; offsetX: number; offsetY: number; hasDragged: boolean};

const clamp = (value: number) => Math.min(1, Math.max(0, value));

const styles = stylex.create({
  // Faint until pointed at, so that it does not hide the remote screen.
  root: {
    position: 'fixed',
    opacity: {default: 0.5, ':hover': 1, ':focus-within': 1},
    transitionProperty: 'opacity',
    transitionDuration: '150ms',
    touchAction: 'none',
    cursor: 'grab',
  },
  dragging: {opacity: 1, cursor: 'grabbing'},
  at: (left: number, top: number) => ({left, top}),
});

function useWindowSize() {
  const [size, setSize] = useState({width: window.innerWidth, height: window.innerHeight});
  useEffect(() => {
    const update = () => setSize({width: window.innerWidth, height: window.innerHeight});
    window.addEventListener('resize', update);
    return () => window.removeEventListener('resize', update);
  }, []);
  return size;
}

/** A small button over the remote screen that opens the session menu. Drag it out of the way. */
export default function SessionMenuButton({onOpen}: {onOpen: () => void}) {
  const rootRef = useRef<HTMLDivElement>(null);
  const pressRef = useRef<Press | null>(null);
  const [position, setPosition] = useState(() => readPreference('menuButton', DEFAULT_POSITION));
  const [isDragging, setIsDragging] = useState(false);
  const viewport = useWindowSize();

  // The button's own size is only known once it is laid out.
  const [size, setSize] = useState({width: 0, height: 0});
  useEffect(() => {
    const rect = rootRef.current?.getBoundingClientRect();
    if (rect) setSize({width: rect.width, height: rect.height});
  }, []);

  const rangeX = Math.max(0, viewport.width - size.width - 2 * MARGIN);
  const rangeY = Math.max(0, viewport.height - size.height - 2 * MARGIN);
  const left = MARGIN + clamp(position.x) * rangeX;
  const top = MARGIN + clamp(position.y) * rangeY;

  const onPointerDown = (event: ReactPointerEvent) => {
    if (event.button !== 0) return;
    pressRef.current = {
      startX: event.clientX,
      startY: event.clientY,
      offsetX: event.clientX - left,
      offsetY: event.clientY - top,
      hasDragged: false,
    };
    // Keep receiving the pointer however fast it leaves the button.
    rootRef.current?.setPointerCapture(event.pointerId);
  };

  const onPointerMove = (event: ReactPointerEvent) => {
    const press = pressRef.current;
    if (!press) return;
    if (!press.hasDragged) {
      const travel = Math.hypot(event.clientX - press.startX, event.clientY - press.startY);
      if (travel < DRAG_THRESHOLD) return;
      press.hasDragged = true;
      setIsDragging(true);
    }
    setPosition({
      x: rangeX > 0 ? clamp((event.clientX - press.offsetX - MARGIN) / rangeX) : 0,
      y: rangeY > 0 ? clamp((event.clientY - press.offsetY - MARGIN) / rangeY) : 0,
    });
  };

  const onPointerUp = () => {
    if (!pressRef.current?.hasDragged) return;
    setIsDragging(false);
    writePreference('menuButton', position);
  };

  const onPointerCancel = () => {
    pressRef.current = null;
    setIsDragging(false);
  };

  // Mouse, touch and keyboard activation all end in a click here; the one
  // that ends a drag is not a request to open the menu.
  const onClick = () => {
    const hasDragged = pressRef.current?.hasDragged;
    pressRef.current = null;
    if (!hasDragged) onOpen();
  };

  return (
    <div
      ref={rootRef}
      {...stylex.props(styles.root, isDragging && styles.dragging, styles.at(left, top))}
      // Pressing the button must not take keyboard focus from the remote screen.
      onMouseDown={event => event.preventDefault()}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onClick={onClick}>
      {/* The card gives the button a surface of its own: remote screens can be any colour. */}
      <Card padding={0} elevation="med">
        <IconButton
          label="Session menu"
          tooltip={isDragging ? undefined : 'Session menu'}
          icon={<Icon icon={Bars3Icon} />}
          variant="ghost"
          size="sm"
        />
      </Card>
    </div>
  );
}
