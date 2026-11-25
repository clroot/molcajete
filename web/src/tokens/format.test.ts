import {describe, expect, test} from 'bun:test';
import type {ApiToken} from '../api';
import {expiresAt, isExpired, parseAllowedIps} from './format';

describe('API tokens', () => {
  test('expire the chosen number of days later', () => {
    expect(expiresAt('30', 1_000)).toBe(1_000 + 30 * 86_400_000);
  });

  test('read allowed addresses one per line or separated by commas', () => {
    expect(parseAllowedIps(' 10.0.0.0/8\n\n192.0.2.7, 2001:db8::/32 ')).toEqual([
      '10.0.0.0/8',
      '192.0.2.7',
      '2001:db8::/32',
    ]);
    expect(parseAllowedIps('  \n ')).toEqual([]);
  });

  test('know when they have expired', () => {
    const token = {expiresAt: 5_000} as ApiToken;
    expect(isExpired(token, 4_999)).toBe(false);
    expect(isExpired(token, 5_000)).toBe(true);
  });
});
