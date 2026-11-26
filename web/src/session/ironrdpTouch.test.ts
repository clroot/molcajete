import {describe, expect, test} from 'bun:test';
import {HOLD_MS, TouchGestures, TWO_FINGER_TAP_MS, type TouchAction} from './ironrdpTouch';

const at = (x: number, y: number) => ({x, y});
const leftClick = (x: number, y: number): TouchAction[] => [
  {kind: 'move', at: at(x, y)},
  {kind: 'button', button: 0, isDown: true},
  {kind: 'button', button: 0, isDown: false},
];

describe('touch on IronRDP sessions', () => {
  test('a tap clicks where the finger was', () => {
    const g = new TouchGestures();
    expect(g.down(1, at(100, 200), 0)).toEqual([]);
    expect(g.move(1, at(103, 202))).toEqual([]);
    expect(g.up(1, 120)).toEqual(leftClick(100, 200));
  });

  test('a drag scrolls by the distance moved, content following the finger', () => {
    const g = new TouchGestures();
    g.down(1, at(100, 300), 0);
    expect(g.move(1, at(100, 280))).toEqual([{kind: 'wheel', dx: 0, dy: 20}]);
    expect(g.move(1, at(100, 250))).toEqual([{kind: 'wheel', dx: 0, dy: 30}]);
    expect(g.move(1, at(100, 270))).toEqual([{kind: 'wheel', dx: 0, dy: -20}]);
    // No click at the end of a scroll, and no hold either.
    expect(g.hold(HOLD_MS)).toEqual([]);
    expect(g.up(1, 900)).toEqual([]);
  });

  test('a resting finger presses the left button and drags with it', () => {
    const g = new TouchGestures();
    g.down(1, at(50, 60), 0);
    expect(g.hold(HOLD_MS - 1)).toEqual([]);
    expect(g.hold(HOLD_MS)).toEqual([
      {kind: 'move', at: at(50, 60)},
      {kind: 'button', button: 0, isDown: true},
    ]);
    expect(g.move(1, at(150, 160))).toEqual([{kind: 'move', at: at(150, 160)}]);
    expect(g.up(1, 2000)).toEqual([{kind: 'button', button: 0, isDown: false}]);
  });

  test('a two-finger tap right-clicks where the first finger was', () => {
    const g = new TouchGestures();
    g.down(1, at(10, 20), 0);
    g.down(2, at(60, 20), 30);
    expect(g.up(2, 150)).toEqual([
      {kind: 'move', at: at(10, 20)},
      {kind: 'button', button: 2, isDown: true},
      {kind: 'button', button: 2, isDown: false},
    ]);
    expect(g.up(1, 160)).toEqual([]);
    // The next touch starts afresh.
    g.down(3, at(5, 5), 1000);
    expect(g.up(3, 1050)).toEqual(leftClick(5, 5));
  });

  test('two fingers that move or stay too long do not right-click', () => {
    const moved = new TouchGestures();
    moved.down(1, at(10, 100), 0);
    moved.down(2, at(60, 100), 10);
    expect(moved.move(1, at(10, 80))).toEqual([{kind: 'wheel', dx: 0, dy: 10}]);
    expect(moved.up(1, 100)).toEqual([]);
    expect(moved.up(2, 110)).toEqual([]);

    const slow = new TouchGestures();
    slow.down(1, at(10, 100), 0);
    slow.down(2, at(60, 100), 10);
    expect(slow.up(1, TWO_FINGER_TAP_MS + 1)).toEqual([]);
  });

  test('a cancelled touch lets go of a held button', () => {
    const g = new TouchGestures();
    g.down(1, at(1, 1), 0);
    g.hold(HOLD_MS);
    expect(g.cancel()).toEqual([{kind: 'button', button: 0, isDown: false}]);
    expect(g.cancel()).toEqual([]);
  });
});
