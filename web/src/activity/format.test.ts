import {describe, expect, test} from 'bun:test';
import {outcome} from './format';

describe('outcome', () => {
  test('names who ended a session before the status', () => {
    expect(outcome({endStatus: 0x020b, endReason: 'admin'})).toEqual({
      label: 'Ended by an administrator',
      variant: 'warning',
    });
    expect(outcome({endStatus: 0x0303, endReason: 'access_revoked'}).label).toBe('Access removed');
  });

  test('tells normal closes from errors', () => {
    expect(outcome({endStatus: 0, endReason: null})).toEqual({label: 'Closed', variant: 'success'});
    expect(outcome({endStatus: 0x0203, endReason: null})).toEqual({
      label: 'Remote desktop error (515)',
      variant: 'error',
    });
    expect(outcome({endStatus: 0x0399, endReason: null}).label).toBe('Error (921)');
  });
});
