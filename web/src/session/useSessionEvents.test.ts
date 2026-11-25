import {describe, expect, test} from 'bun:test';
import type {SessionState} from './useSessionEvents';
import {reduce} from './useSessionEvents';

const connected: SessionState = {
  status: {kind: 'connected'},
  required: null,
  clipboard: '',
  isUnstable: false,
  stats: null,
};

describe('session events', () => {
  test('keeps the state for events the page does not use', () => {
    // Audio events reached the reducer and replaced the state with
    // undefined, which blanked the page.
    for (const type of ['audio-start', 'audio-data', 'audio-end', 'something-new']) {
      const action = {type, detail: {stream: 128}} as unknown as Parameters<typeof reduce>[1];
      expect(reduce(connected, action)).toBe(connected);
    }
  });

  test('a disconnect keeps the error that caused it', () => {
    const failed = reduce(connected, {type: 'error', detail: {message: 'gone', name: 'UPSTREAM_ERROR'}});
    expect(reduce(failed, {type: 'state', detail: 'disconnected'}).status.kind).toBe('failed');
  });
});
