import {useEffect, useState} from 'react';

// Hash routes, as in the original client: the gateway serves one page and
// the fragment never reaches the server.
export type Route =
  | {name: 'connections'}
  | {name: 'users'}
  | {name: 'groups'}
  | {name: 'recordings'}
  | {name: 'audit'}
  | {name: 'activeSessions'}
  | {name: 'history'}
  | {name: 'tokens'}
  | {name: 'ticket'; ticket: string}
  | {name: 'session'; connectionId: string}
  | {name: 'join'; token: string}
  | {name: 'watch'; sessionId: string}
  | {name: 'playback'; recordingId: string};

export function parseRoute(hash: string): Route {
  const session = hash.match(/^#\/connect\/(.+)$/);
  if (session) return {name: 'session', connectionId: decodeURIComponent(session[1])};
  const join = hash.match(/^#\/join\/(.+)$/);
  if (join) return {name: 'join', token: decodeURIComponent(join[1])};
  const watch = hash.match(/^#\/watch\/(.+)$/);
  if (watch) return {name: 'watch', sessionId: decodeURIComponent(watch[1])};
  const playback = hash.match(/^#\/recordings\/(.+)$/);
  if (playback) return {name: 'playback', recordingId: decodeURIComponent(playback[1])};
  const ticket = hash.match(/^#\/ticket\/(.+)$/);
  if (ticket) return {name: 'ticket', ticket: decodeURIComponent(ticket[1])};
  if (hash === '#/users') return {name: 'users'};
  if (hash === '#/groups') return {name: 'groups'};
  if (hash === '#/recordings') return {name: 'recordings'};
  if (hash === '#/audit') return {name: 'audit'};
  if (hash === '#/sessions') return {name: 'activeSessions'};
  if (hash === '#/history') return {name: 'history'};
  if (hash === '#/tokens') return {name: 'tokens'};
  return {name: 'connections'};
}

export const href = {
  connections: '#/',
  users: '#/users',
  groups: '#/groups',
  recordings: '#/recordings',
  audit: '#/audit',
  activeSessions: '#/sessions',
  history: '#/history',
  tokens: '#/tokens',
  playback: (recordingId: string) => `#/recordings/${encodeURIComponent(recordingId)}`,
  session: (connectionId: string) => `#/connect/${encodeURIComponent(connectionId)}`,
  watch: (sessionId: string) => `#/watch/${encodeURIComponent(sessionId)}`,
};

export function navigate(to: string) {
  location.hash = to;
}

export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseRoute(location.hash));
  useEffect(() => {
    const update = () => setRoute(parseRoute(location.hash));
    window.addEventListener('hashchange', update);
    return () => window.removeEventListener('hashchange', update);
  }, []);
  return route;
}
