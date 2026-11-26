import {describe, expect, test} from 'bun:test';
import {KEYSYM, SYMBOLS, isKeyboardOpen, nextHold, showsKeyBar} from './keyBar';

describe('terminal key bar', () => {
  test('a tap holds a modifier for one key, the next locks it, the third lets go', () => {
    expect(nextHold('off')).toBe('once');
    expect(nextHold('once')).toBe('locked');
    expect(nextHold('locked')).toBe('off');
  });

  test('shows on touch devices unless the user chose otherwise', () => {
    expect(showsKeyBar(null, true)).toBe(true);
    expect(showsKeyBar(null, false)).toBe(false);
    // A tablet with a keyboard, a laptop with a touch screen.
    expect(showsKeyBar(false, true)).toBe(false);
    expect(showsKeyBar(true, false)).toBe(true);
  });

  test('takes the keyboard as open when it covers more than an address bar would', () => {
    // A 393×852 phone upright: Safari's address bar, then the keyboard.
    expect(isKeyboardOpen(852, 852)).toBe(false);
    expect(isKeyboardOpen(852 - 80, 852)).toBe(false);
    expect(isKeyboardOpen(852 - 336, 852)).toBe(true);
    // On its side, with the keyboard taking most of the height.
    expect(isKeyboardOpen(393 - 200, 393)).toBe(true);
  });

  test('sends the keysyms terminals expect', () => {
    expect(KEYSYM.tab).toBe(0xff09);
    expect(KEYSYM.escape).toBe(0xff1b);
    expect([KEYSYM.left, KEYSYM.up, KEYSYM.right, KEYSYM.down]).toEqual([0xff51, 0xff52, 0xff53, 0xff54]);
    // Latin-1 characters are their own keysyms.
    expect(SYMBOLS.map(s => s.codePointAt(0))).toEqual([0x7c, 0x2f, 0x2d, 0x7e]);
  });
});
