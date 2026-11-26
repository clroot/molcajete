import {useSyncExternalStore} from 'react';

/** The part of the window that an on-screen keyboard leaves in view, in CSS pixels. */
export type VisibleFrame = {top: number; height: number};

function subscribe(notify: () => void) {
  const viewport = window.visualViewport;
  if (!viewport) return () => {};
  viewport.addEventListener('resize', notify);
  viewport.addEventListener('scroll', notify);
  return () => {
    viewport.removeEventListener('resize', notify);
    viewport.removeEventListener('scroll', notify);
  };
}

/** The frame as one value, so that an unchanged frame is an unchanged snapshot. */
function snapshot(): string {
  const viewport = window.visualViewport;
  // A pinch-zoomed page shows a magnified part of itself, not what a
  // keyboard left over.
  if (!viewport || Math.abs(viewport.scale - 1) > 0.01) return '';
  return `${Math.round(viewport.offsetTop)}:${Math.floor(viewport.height)}`;
}

/**
 * Follows `window.visualViewport` while `isEnabled`. Browsers on phones keep
 * the layout viewport when the on-screen keyboard opens and shrink only the
 * visual one, so a page laid out in the window ends up under the keyboard.
 * `null` when there is nothing to follow.
 */
export function useVisibleFrame(isEnabled: boolean): VisibleFrame | null {
  const frame = useSyncExternalStore(subscribe, snapshot);
  if (!isEnabled || frame === '') return null;
  const [top, height] = frame.split(':').map(Number);
  return {top, height};
}
