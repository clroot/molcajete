import {describe, expect, test} from 'bun:test';
import type {SessionState} from './useSessionEvents';
import {reduce} from './useSessionEvents';

const connected: SessionState = {
  status: {kind: 'connected'},
  required: null,
  clipboard: '',
  isUnstable: false,
  stats: null,
  canChangeDisplay: true,
  link: null,
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

  test('the server can say it cannot change the display, until the next connection', () => {
    const fixed = reduce(connected, {type: 'display-control', detail: false});
    expect(fixed.canChangeDisplay).toBe(false);
    expect(reduce(fixed, {type: 'display-control', detail: true}).canChangeDisplay).toBe(true);
    expect(reduce(fixed, {type: 'state', detail: 'connecting'}).canChangeDisplay).toBe(true);
  });

  test('keeps the link under the pointer until it leaves or the session reconnects', () => {
    const over = reduce(connected, {type: 'link', detail: 'https://example.com'});
    expect(over.link).toBe('https://example.com');
    expect(reduce(over, {type: 'link', detail: null}).link).toBeNull();
    expect(reduce(over, {type: 'state', detail: 'connecting'}).link).toBeNull();
  });

  test('a disconnect keeps the error that caused it', () => {
    const failed = reduce(connected, {type: 'error', detail: {message: 'gone', name: 'UPSTREAM_ERROR'}});
    expect(reduce(failed, {type: 'state', detail: 'disconnected'}).status.kind).toBe('failed');
  });
});
