import {describe, expect, test} from 'bun:test';
import {formatClock, formatSize} from './format';

describe('recording text', () => {
  test('formats positions like a player', () => {
    expect(formatClock(0)).toBe('0:00');
    expect(formatClock(59_999)).toBe('0:59');
    expect(formatClock(123_000)).toBe('2:03');
    expect(formatClock(3_723_000)).toBe('1:02:03');
    expect(formatClock(-5)).toBe('0:00');
  });

  test('formats sizes', () => {
    expect(formatSize(512)).toBe('512 B');
    expect(formatSize(1_500_000)).toBe('1.5 MB');
    expect(formatSize(12_300_000_000)).toBe('12 GB');
  });
});
