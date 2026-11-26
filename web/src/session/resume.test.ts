import {describe, expect, test} from 'bun:test';
import {shouldReconnect} from './resume';

describe('resume', () => {
  test('reconnects after a dropped tunnel, not after the session ended', () => {
    // The client's own timeout, a network drop, a gateway restart.
    for (const code of [0x0202, 0x0203, 0x0207, 0x0000]) expect(shouldReconnect(code)).toBe(true);
    // Ended by an administrator, access removed, signed out, taken over elsewhere.
    for (const code of [0x020b, 0x0303, 0x0301, 0x0209]) expect(shouldReconnect(code)).toBe(false);
  });
});
