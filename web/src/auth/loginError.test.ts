import {expect, test} from 'bun:test';
import {loginErrorMessage} from './loginError';

test('known codes have their own message', () => {
  expect(loginErrorMessage('disabled')).toContain('disabled');
  expect(loginErrorMessage('cancelled')).toBe('Sign-in was cancelled.');
});

test('unknown codes fall back to a generic message', () => {
  expect(loginErrorMessage('<script>')).toBe(loginErrorMessage('failed'));
});
