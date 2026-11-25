import {useReducer} from 'react';
import type {SessionEvent, SessionStats} from './client';

export type ConnectionStatus =
  | {kind: 'connecting'}
  | {kind: 'connected'}
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
};

type Action =
  | SessionEvent
  | {type: 'reconnecting'}
  | {type: 'answered'}
  | {type: 'failed'; title: string; detail: string};

const initialState: SessionState = {
  status: {kind: 'connecting'},
  required: null,
  clipboard: '',
  isUnstable: false,
  stats: null,
};

function reduce(state: SessionState, action: Action): SessionState {
  switch (action.type) {
    case 'state':
      if (action.detail === 'connecting' || action.detail === 'waiting')
        return {...state, status: {kind: 'connecting'}};
      if (action.detail === 'connected') return {...state, status: {kind: 'connected'}};
      // Keep the error that caused the disconnect on screen.
      if (action.detail === 'disconnected' && state.status.kind !== 'failed') {
        return {...state, status: {kind: 'disconnected'}};
      }
      return state;
    case 'error':
      return {
        ...state,
        status: {
          kind: 'failed',
          title: 'Connection error',
          detail: `${action.detail.message} (${action.detail.name})`,
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
    case 'reconnecting':
      return {...state, status: {kind: 'connecting'}, required: null};
    case 'name':
      // guacd reports the protocol's own name ("vnc"); the page shows the
      // configured connection name instead.
      return state;
  }
}

/** Session UI state, driven by the client's events. */
export function useSessionEvents() {
  return useReducer(reduce, initialState);
}
