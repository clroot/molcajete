// The terminal key bar: the keys a phone's on-screen keyboard lacks.
import type {Hold} from './client';

/** X11 keysyms of the keys the bar sends. A printable character is its code point. */
export const KEYSYM = {
  escape: 0xff1b,
  tab: 0xff09,
  left: 0xff51,
  up: 0xff52,
  right: 0xff53,
  down: 0xff54,
  home: 0xff50,
  end: 0xff57,
  pageUp: 0xff55,
  pageDown: 0xff56,
} as const;

/** Shell characters that sit two layers deep on an on-screen keyboard. */
export const SYMBOLS = ['|', '/', '-', '~'] as const;

/** What a tap on Ctrl or Alt does: hold it for one key, then lock it, then let go. */
export function nextHold(hold: Hold): Hold {
  if (hold === 'off') return 'once';
  return hold === 'once' ? 'locked' : 'off';
}

/**
 * Whether the bar shows: what the user chose in this browser, else on a
 * device whose main pointer is a finger.
 */
export const showsKeyBar = (choice: boolean | null, isCoarsePointer: boolean) => choice ?? isCoarsePointer;
