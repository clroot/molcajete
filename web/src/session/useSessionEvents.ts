import {useReducer} from 'react';
import type {HeldModifiers, SessionEvent, SessionStats, VideoMode} from './client';

export type ConnectionStatus =
  | {kind: 'connecting'}
  | {kind: 'connected'}
  /** The tunnel dropped and the page connects again; `error` is why the last attempt failed. */
  | {kind: 'reconnecting'; error?: ConnectionStatus}
  | {kind: 'disconnected'}
  | {kind: 'failed'; title: string; detail: string};

export type SessionState = {
  status: ConnectionStatus;
  /** Parameters the server asked for (`required`), or null. */
  required: string[] | null;
  /** Latest remote clipboard text. */
  clipboard: string;
  isUnstable: boolean;
  stats: SessionStats | null;
  /** False once the server said it cannot change the remote display during the session. */
  canChangeDisplay: boolean;
  /** Target of the link under the pointer in a terminal. */
  link: string | null;
  /** The session's video mode, or null when it streams no video (or the server is not molcajeted). */
  videoMode: VideoMode | null;
  /** An audio stream is open, but the browser plays nothing until a click or key press. */
  isAudioBlocked: boolean;
  /** Ctrl and Alt as the terminal key bar holds them. */
  modifiers: HeldModifiers;
};

type Action =
  | SessionEvent
  | {type: 'reconnecting'}
  /** The page connects again by itself after the tunnel dropped. */
  | {type: 'resuming'}
  /** It stopped trying: what the last attempt failed with stays on screen. */
  | {type: 'gave-up'}
  | {type: 'answered'}
  | {type: 'failed'; title: string; detail: string};

const NO_MODIFIERS: HeldModifiers = {ctrl: 'off', alt: 'off'};

const initialState: SessionState = {
  status: {kind: 'connecting'},
  required: null,
  clipboard: '',
  isUnstable: false,
  stats: null,
  canChangeDisplay: true,
  link: null,
  videoMode: null,
  isAudioBlocked: false,
  modifiers: NO_MODIFIERS,
};

/** Statuses the gateway closes a tunnel with when it ends the session itself. */
const ENDED_BY_GATEWAY: Record<number, {title: string; detail: string}> = {
  // SESSION_CLOSED
  0x020b: {title: 'Ended by an administrator', detail: 'An administrator ended this session.'},
  // CLIENT_FORBIDDEN
  0x0303: {
    title: 'Access removed',
    detail: 'You can no longer use this connection. Ask an administrator if you need it.',
  },
};

function errorStatus(error: {message: string; name: string; code?: number}): ConnectionStatus {
  // Only the tunnel's close carries the gateway's own status; an `error`
  // from the remote desktop with the same code means something else.
  const ended =
    error.message.startsWith('tunnel closed') && error.code !== undefined
      ? ENDED_BY_GATEWAY[error.code]
      : undefined;
  if (ended) return {kind: 'failed', ...ended};
  // molcajeted: another window took the session over.
  if (error.code === 0x0209 && !error.message.startsWith('tunnel closed'))
    return {kind: 'failed', title: 'Opened in another window', detail: error.message};
  return {kind: 'failed', title: 'Connection error', detail: `${error.message} (${error.name})`};
}

export function reduce(state: SessionState, action: Action): SessionState {
  const isReconnecting = state.status.kind === 'reconnecting';
  switch (action.type) {
    case 'state':
      // A new connection may reach a different server.
      if (action.detail === 'connecting')
        return {
          ...state,
          status: isReconnecting ? state.status : {kind: 'connecting'},
          canChangeDisplay: true,
          link: null,
          modifiers: NO_MODIFIERS,
          videoMode: null,
        };
      if (action.detail === 'waiting' && !isReconnecting) return {...state, status: {kind: 'connecting'}};
      if (action.detail === 'connected') return {...state, status: {kind: 'connected'}};
      // Keep the error that caused the disconnect on screen.
      if (action.detail === 'disconnected' && state.status.kind !== 'failed' && !isReconnecting) {
        return {...state, status: {kind: 'disconnected'}};
      }
      return state;
    case 'error':
      // Kept for when the page stops trying.
      if (state.status.kind === 'reconnecting')
        return {...state, status: {kind: 'reconnecting', error: errorStatus(action.detail)}};
      return {...state, status: errorStatus(action.detail)};
    case 'resuming':
      return {
        ...state,
        status: {kind: 'reconnecting', error: state.status.kind === 'failed' ? state.status : undefined},
        required: null,
      };
    case 'gave-up':
      if (state.status.kind !== 'reconnecting') return state;
      return {
        ...state,
        status: state.status.error ?? {
          kind: 'failed',
          title: 'Connection lost',
          detail: 'The connection to the server dropped and could not be restored.',
        },
      };
    case 'failed':
      return {...state, status: {kind: 'failed', title: action.title, detail: action.detail}};
    case 'clipboard':
      return {...state, clipboard: action.detail};
    case 'required':
      return {...state, required: action.detail};
    case 'answered':
      return {...state, required: null};
    case 'unstable':
      return {...state, isUnstable: action.detail};
    case 'stats':
      return {...state, stats: action.detail};
    case 'display-control':
      return {...state, canChangeDisplay: action.detail};
    case 'video-mode':
      return {...state, videoMode: action.detail};
    case 'link':
      return {...state, link: action.detail};
    case 'audio':
      return {...state, isAudioBlocked: action.detail.blocked};
    case 'modifiers':
      return {...state, modifiers: action.detail};
    case 'reconnecting':
      return {...state, status: {kind: 'connecting'}, required: null};
    case 'name':
      // guacd reports the protocol's own name ("vnc"); the page shows the
      // configured connection name instead.
      return state;
    default:
      // The client may report events this page does not use.
      return state;
  }
}

/** Session UI state, driven by the client's events. */
export function useSessionEvents() {
  return useReducer(reduce, initialState);
}
