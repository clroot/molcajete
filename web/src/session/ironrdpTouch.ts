// Touch for IronRDP's web client, which reads only the mouse and the
// wheel. Without this, a finger on the remote screen of an iPad or a phone
// scrolls the page instead (and pulls it down to reload).
//
// - one finger dragged: wheel movement of the same distance (scrolls)
// - one finger tapped: a left click where it was
// - one finger resting for HOLD_MS: the left button goes down there, and
//   the finger drags with it until it is lifted (selecting, moving windows)
// - two fingers tapped: a right click where the first one was

/** Finger travel, in CSS pixels, after which a touch is a drag and not a tap. */
export const SLOP = 8;
/** A finger that rests this long presses the left button. */
export const HOLD_MS = 500;
/** A two-finger touch lifted within this long, without moving, is a right click. */
export const TWO_FINGER_TAP_MS = 400;

export type Point = {x: number; y: number};

export type TouchAction =
  | {kind: 'move'; at: Point}
  | {kind: 'button'; button: 0 | 2; isDown: boolean}
  | {kind: 'wheel'; dx: number; dy: number};

type State =
  | {kind: 'idle'}
  /** One finger down, not yet a tap, a drag or a hold. */
  | {kind: 'pending'; id: number; start: Point; startedAt: number}
  | {kind: 'scrolling'; ids: Set<number>; last: Map<number, Point>}
  | {kind: 'pressing'; id: number}
  | {
      kind: 'twoFingers';
      ids: Set<number>;
      first: Point;
      last: Map<number, Point>;
      startedAt: number;
      hasMoved: boolean;
    }
  /** Fingers still down after a gesture ended; ignored until all are lifted. */
  | {kind: 'over'; ids: Set<number>};

const distance = (a: Point, b: Point) => Math.hypot(a.x - b.x, a.y - b.y);
const click = (at: Point, button: 0 | 2): TouchAction[] => [
  {kind: 'move', at},
  {kind: 'button', button, isDown: true},
  {kind: 'button', button, isDown: false},
];

/** Turns touches into mouse actions. Times are in milliseconds, points in client pixels. */
export class TouchGestures {
  #state: State = {kind: 'idle'};

  down(id: number, at: Point, now: number): TouchAction[] {
    const s = this.#state;
    switch (s.kind) {
      case 'idle':
        this.#state = {kind: 'pending', id, start: at, startedAt: now};
        return [];
      case 'pending':
        this.#state = {
          kind: 'twoFingers',
          ids: new Set([s.id, id]),
          first: s.start,
          last: new Map([
            [s.id, s.start],
            [id, at],
          ]),
          startedAt: s.startedAt,
          hasMoved: false,
        };
        return [];
      case 'scrolling':
        s.ids.add(id);
        s.last.set(id, at);
        return [];
      case 'pressing':
        this.#state = {kind: 'over', ids: new Set([s.id, id])};
        return [{kind: 'button', button: 0, isDown: false}];
      case 'twoFingers':
      case 'over':
        this.#state = {kind: 'over', ids: new Set([...s.ids, id])};
        return [];
    }
  }

  move(id: number, at: Point): TouchAction[] {
    const s = this.#state;
    switch (s.kind) {
      case 'pending': {
        if (id !== s.id || distance(at, s.start) <= SLOP) return [];
        this.#state = {kind: 'scrolling', ids: new Set([id]), last: new Map([[id, at]])};
        return [this.#wheel(s.start, at)];
      }
      case 'scrolling':
      case 'twoFingers': {
        const last = s.last.get(id);
        if (!last) return [];
        s.last.set(id, at);
        if (s.kind === 'twoFingers') {
          if (!s.hasMoved && distance(at, last) <= SLOP / 2 && distance(at, s.first) <= SLOP) return [];
          s.hasMoved = true;
        }
        // Several fingers scroll by their average movement.
        return [this.#wheel(last, at, s.last.size)];
      }
      case 'pressing':
        return id === s.id ? [{kind: 'move', at}] : [];
      default:
        return [];
    }
  }

  up(id: number, now: number): TouchAction[] {
    const s = this.#state;
    switch (s.kind) {
      case 'pending':
        this.#state = {kind: 'idle'};
        return id === s.id ? click(s.start, 0) : [];
      case 'pressing':
        this.#state = {kind: 'idle'};
        return [{kind: 'button', button: 0, isDown: false}];
      case 'twoFingers': {
        const isTap = !s.hasMoved && now - s.startedAt <= TWO_FINGER_TAP_MS;
        s.ids.delete(id);
        this.#state = {kind: 'over', ids: s.ids};
        if (s.ids.size === 0) this.#state = {kind: 'idle'};
        return isTap ? click(s.first, 2) : [];
      }
      case 'scrolling':
      case 'over':
        s.ids.delete(id);
        if (s.kind === 'scrolling') s.last.delete(id);
        if (s.ids.size === 0) this.#state = {kind: 'idle'};
        return [];
      default:
        return [];
    }
  }

  /** Called HOLD_MS after a touch started: a finger still resting presses the button. */
  hold(now: number): TouchAction[] {
    const s = this.#state;
    if (s.kind !== 'pending' || now - s.startedAt < HOLD_MS) return [];
    this.#state = {kind: 'pressing', id: s.id};
    return [
      {kind: 'move', at: s.start},
      {kind: 'button', button: 0, isDown: true},
    ];
  }

  /** All fingers gone (the touch was cancelled): a held button is let go. */
  cancel(): TouchAction[] {
    const wasPressing = this.#state.kind === 'pressing';
    this.#state = {kind: 'idle'};
    return wasPressing ? [{kind: 'button', button: 0, isDown: false}] : [];
  }

  /** Content follows the finger: moving it up scrolls down. */
  #wheel(from: Point, to: Point, fingers = 1): TouchAction {
    return {kind: 'wheel', dx: (from.x - to.x) / fingers, dy: (from.y - to.y) / fingers};
  }
}

/**
 * Handles touches on IronRDP's component. Its screen is a canvas in an open
 * shadow root, which listens for mouse and wheel events, so the gestures go
 * to it as such. Returns a function that removes the handlers.
 */
export function installIronRdpTouch(component: HTMLElement): () => void {
  const gestures = new TouchGestures();
  let holdTimer: ReturnType<typeof setTimeout> | undefined;
  let lastAt: Point = {x: 0, y: 0};
  const target = () => component.shadowRoot?.querySelector('canvas') ?? null;

  const apply = (actions: TouchAction[]) => {
    const canvas = target();
    if (!canvas) return;
    for (const action of actions) {
      if (action.kind === 'move') lastAt = action.at;
      const init = {bubbles: true, composed: true, cancelable: true, clientX: lastAt.x, clientY: lastAt.y};
      if (action.kind === 'move') {
        canvas.dispatchEvent(new MouseEvent('mousemove', init));
      } else if (action.kind === 'button') {
        const buttons = action.isDown ? (action.button === 0 ? 1 : 2) : 0;
        canvas.dispatchEvent(
          new MouseEvent(action.isDown ? 'mousedown' : 'mouseup', {...init, button: action.button, buttons}),
        );
      } else {
        canvas.dispatchEvent(
          new WheelEvent('wheel', {
            ...init,
            deltaX: action.dx,
            deltaY: action.dy,
            deltaMode: WheelEvent.DOM_DELTA_PIXEL,
          }),
        );
      }
    }
  };

  const point = (touch: Touch): Point => ({x: touch.clientX, y: touch.clientY});
  const onStart = (event: TouchEvent) => {
    // No page scrolling or zooming, and no mouse events of the browser's own.
    event.preventDefault();
    target()?.focus({preventScroll: true});
    for (const touch of event.changedTouches)
      apply(gestures.down(touch.identifier, point(touch), event.timeStamp));
    clearTimeout(holdTimer);
    holdTimer = setTimeout(() => apply(gestures.hold(performance.now())), HOLD_MS);
  };
  const onMove = (event: TouchEvent) => {
    event.preventDefault();
    for (const touch of event.changedTouches) apply(gestures.move(touch.identifier, point(touch)));
  };
  const onEnd = (event: TouchEvent) => {
    event.preventDefault();
    for (const touch of event.changedTouches) apply(gestures.up(touch.identifier, event.timeStamp));
  };
  const onCancel = () => {
    clearTimeout(holdTimer);
    apply(gestures.cancel());
  };

  const options = {passive: false};
  component.addEventListener('touchstart', onStart, options);
  component.addEventListener('touchmove', onMove, options);
  component.addEventListener('touchend', onEnd, options);
  component.addEventListener('touchcancel', onCancel);
  return () => {
    clearTimeout(holdTimer);
    component.removeEventListener('touchstart', onStart);
    component.removeEventListener('touchmove', onMove);
    component.removeEventListener('touchend', onEnd);
    component.removeEventListener('touchcancel', onCancel);
  };
}

/**
 * Keeps the page from scrolling, bouncing or being pulled down to reload
 * while an IronRDP session is open, wherever a finger starts: on the
 * remote screen, at its edges or on an overlay. Only dialogs (the session
 * menu) still scroll. Taps are not affected. Returns a function that
 * undoes it.
 */
export function blockPageScroll(doc: Document = document): () => void {
  const roots = [doc.documentElement, doc.body];
  const before = roots.map(el => el.style.overscrollBehavior);
  for (const el of roots) el.style.overscrollBehavior = 'none';
  const onMove = (event: TouchEvent) => {
    const inDialog = event.composedPath().some(node => node instanceof HTMLDialogElement);
    if (!inDialog && event.cancelable) event.preventDefault();
  };
  doc.addEventListener('touchmove', onMove, {passive: false, capture: true});
  return () => {
    doc.removeEventListener('touchmove', onMove, {capture: true});
    roots.forEach((el, i) => (el.style.overscrollBehavior = before[i]));
  };
}
