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
  videoMode: null,
  isAudioBlocked: false,
  modifiers: {ctrl: 'off', alt: 'off'},
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

  test('follows the modifiers the key bar holds, until the next connection', () => {
    const held = reduce(connected, {type: 'modifiers', detail: {ctrl: 'once', alt: 'off'}});
    expect(held.modifiers).toEqual({ctrl: 'once', alt: 'off'});
    expect(reduce(held, {type: 'state', detail: 'connecting'}).modifiers).toEqual({ctrl: 'off', alt: 'off'});
  });

  test('says when sound waits for a click', () => {
    const blocked = reduce(connected, {type: 'audio', detail: {blocked: true}});
    expect(blocked.isAudioBlocked).toBe(true);
    expect(reduce(blocked, {type: 'audio', detail: {blocked: false}}).isAudioBlocked).toBe(false);
  });

  test('the server can say it cannot change the display, until the next connection', () => {
    const fixed = reduce(connected, {type: 'display-control', detail: false});
    expect(fixed.canChangeDisplay).toBe(false);
    expect(reduce(fixed, {type: 'display-control', detail: true}).canChangeDisplay).toBe(true);
    expect(reduce(fixed, {type: 'state', detail: 'connecting'}).canChangeDisplay).toBe(true);
  });

  test('offers the video mode the server reports, until video is off or the session reconnects', () => {
    const auto = reduce(connected, {type: 'video-mode', detail: 'auto'});
    expect(auto.videoMode).toBe('auto');
    const full = reduce(auto, {type: 'video-mode', detail: 'full-screen'});
    expect(full.videoMode).toBe('full-screen');
    expect(reduce(full, {type: 'video-mode', detail: null}).videoMode).toBeNull();
    expect(reduce(full, {type: 'state', detail: 'connecting'}).videoMode).toBeNull();
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

  test('says when the gateway ended the session and why', () => {
    const ended = (code: number, message = `tunnel closed: X`) =>
      reduce(connected, {type: 'error', detail: {message, name: 'X', code}}).status;
    expect(ended(0x020b)).toEqual({
      kind: 'failed',
      title: 'Ended by an administrator',
      detail: 'An administrator ended this session.',
    });
    expect(ended(0x0303)).toMatchObject({title: 'Access removed'});
    // The same code from the remote desktop is an ordinary error.
    expect(ended(0x0303, 'denied')).toMatchObject({title: 'Connection error'});
    expect(ended(0x0203)).toMatchObject({title: 'Connection error'});
  });

  test('stays on reconnecting through the attempts, until connected or given up', () => {
    // The tunnel drops: error, then the page starts reconnecting.
    const dropped = reduce(connected, {
      type: 'error',
      detail: {message: 'tunnel closed: UPSTREAM_TIMEOUT', name: 'UPSTREAM_TIMEOUT', code: 0x0202},
    });
    let state = reduce(dropped, {type: 'resuming'});
    expect(state.status.kind).toBe('reconnecting');
    for (const detail of ['disconnected', 'connecting', 'waiting'] as const) {
      state = reduce(state, {type: 'state', detail});
      expect(state.status.kind).toBe('reconnecting');
    }
    // A failed attempt is kept for later.
    state = reduce(state, {
      type: 'error',
      detail: {message: 'tunnel closed: UPSTREAM_NOT_FOUND', name: 'UPSTREAM_NOT_FOUND', code: 0x0207},
    });
    expect(state.status.kind).toBe('reconnecting');
    expect(reduce(state, {type: 'state', detail: 'connected'}).status).toEqual({kind: 'connected'});
    const gaveUp = reduce(state, {type: 'gave-up'});
    expect(gaveUp.status.kind).toBe('failed');
    expect(gaveUp.status.kind === 'failed' && gaveUp.status.detail).toContain('UPSTREAM_NOT_FOUND');
  });

  test('says so when another window took the session', () => {
    const state = reduce(connected, {
      type: 'error',
      detail: {message: 'The session was opened in another window', name: 'SESSION_CONFLICT', code: 0x0209},
    });
    expect(state.status).toEqual({
      kind: 'failed',
      title: 'Opened in another window',
      detail: 'The session was opened in another window',
    });
  });
});
