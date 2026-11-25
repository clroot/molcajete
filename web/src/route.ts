import {useEffect, useState} from 'react';

// Hash routes, as in the original client: the gateway serves one page and
// the fragment never reaches the server.
export type Route = {name: 'connections'} | {name: 'users'} | {name: 'session'; connectionId: string};

export function parseRoute(hash: string): Route {
  const session = hash.match(/^#\/connect\/(.+)$/);
  if (session) return {name: 'session', connectionId: decodeURIComponent(session[1])};
  if (hash === '#/users') return {name: 'users'};
  return {name: 'connections'};
}

export const href = {
  connections: '#/',
  users: '#/users',
  session: (connectionId: string) => `#/connect/${encodeURIComponent(connectionId)}`,
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
