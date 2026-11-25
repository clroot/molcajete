import {useEffect, useState} from 'react';
import {api} from '../api';
import type {ConnectionDisplay} from './displaySettings';

type ConnectionInfo = {name: string; protocol: string | null; display: ConnectionDisplay};

/**
 * The connection's configured name (the id until it loads), protocol
 * (`null` until it loads) and display parameters; also sets the tab title.
 */
export function useConnection(connectionId: string) {
  const [connection, setConnection] = useState<ConnectionInfo>({
    name: connectionId,
    protocol: null,
    display: {dpi: null, resizeMethod: null},
  });
  useEffect(() => {
    let isCurrent = true;
    api.connections().then(
      list => {
        const found = list.find(c => c.id === connectionId);
        if (isCurrent && found) {
          setConnection({
            name: found.name,
            protocol: found.protocol,
            // Older gateways do not send them.
            display: {dpi: found.dpi ?? null, resizeMethod: found.resizeMethod ?? null},
          });
        }
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
