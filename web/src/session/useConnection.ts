import {useEffect, useState} from 'react';
import {api} from '../api';
import type {ConnectionDisplay} from './displaySettings';

/** What the connection turns off for its users. */
export type Restrictions = {readOnly: boolean; disableCopy: boolean; disablePaste: boolean};

type ConnectionInfo = {
  name: string;
  protocol: string | null;
  display: ConnectionDisplay;
  restrictions: Restrictions;
};

/**
 * The connection's configured name (the id until it loads), protocol
 * (`null` until it loads), display parameters and restrictions; also sets
 * the tab title.
 */
export function useConnection(connectionId: string) {
  const [connection, setConnection] = useState<ConnectionInfo>({
    name: connectionId,
    protocol: null,
    display: {dpi: null, resizeMethod: null},
    restrictions: {readOnly: false, disableCopy: false, disablePaste: false},
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
            restrictions: {
              readOnly: found.readOnly ?? false,
              disableCopy: found.disableCopy ?? false,
              disablePaste: found.disablePaste ?? false,
            },
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
