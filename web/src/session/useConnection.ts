import {useEffect, useState} from 'react';
import {api} from '../api';

/**
 * The connection's configured name (the id until it loads) and protocol
 * (`null` until it loads); also sets the tab title.
 */
export function useConnection(connectionId: string) {
  const [connection, setConnection] = useState<{name: string; protocol: string | null}>({
    name: connectionId,
    protocol: null,
  });
  useEffect(() => {
    let isCurrent = true;
    api.connections().then(
      list => {
        const found = list.find(c => c.id === connectionId);
        if (isCurrent && found) setConnection({name: found.name, protocol: found.protocol});
      },
      () => {}, // Keep showing the id.
    );
    return () => {
      isCurrent = false;
    };
  }, [connectionId]);

  const {name} = connection;
  useEffect(() => {
    document.title = `${name} · molcajete`;
    return () => {
      document.title = 'molcajete';
    };
  }, [name]);

  return connection;
}
