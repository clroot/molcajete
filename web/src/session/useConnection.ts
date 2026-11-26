import {useEffect, useState} from 'react';
import {api} from '../api';
import type {ConnectionDisplay} from './displaySettings';

/** What the connection turns off for its users. */
export type Restrictions = {readOnly: boolean; disableCopy: boolean; disablePaste: boolean};

/** Which RDP client sessions open with, and whether the session menu switches. */
export type RdpClientChoice = {client: 'molcajete' | 'ironrdp'; canSwitch: boolean};

export type ConnectionInfo = {
  name: string;
  protocol: string | null;
  display: ConnectionDisplay;
  restrictions: Restrictions;
  /** Older gateways, and one-time links, give molcajete's client only. */
  rdpClient?: RdpClientChoice;
};

/**
 * The connection's configured name (the id until it loads), protocol
 * (`null` until it loads), display parameters and restrictions; also sets
 * the tab title. `known` is used as it is instead of asking the API (a
 * session opened by a one-time link has no sign-in to ask with).
 */
export function useConnection(
  connectionId: string,
  known?: ConnectionInfo,
): ConnectionInfo & {isLoaded: boolean} {
  const [connection, setConnection] = useState<ConnectionInfo>(
    () =>
      known ?? {
        name: connectionId,
        protocol: null,
        display: {dpi: null, resizeMethod: null},
        restrictions: {readOnly: false, disableCopy: false, disablePaste: false},
      },
  );
  const isKnown = known !== undefined;
  const [isLoaded, setIsLoaded] = useState(isKnown);
  useEffect(() => {
    if (isKnown) return;
    let isCurrent = true;
    api.connections().then(
      list => {
        const found = list.find(c => c.id === connectionId);
        if (isCurrent && found) {
          setConnection({
            name: found.name,
            protocol: found.protocol,
            // Older gateways do not send them.
            display: {
              dpi: found.dpi ?? null,
              resizeMethod: found.resizeMethod ?? null,
              fontSize: found.fontSize ?? null,
            },
            restrictions: {
              readOnly: found.readOnly ?? false,
              disableCopy: found.disableCopy ?? false,
              disablePaste: found.disablePaste ?? false,
            },
            rdpClient: {
              client: found.ironrdpDefault ? 'ironrdp' : 'molcajete',
              canSwitch: found.ironrdpSwitch ?? false,
            },
          });
        }
        if (isCurrent) setIsLoaded(true);
      },
      // Keep showing the id, and open the session with molcajete's client.
      () => isCurrent && setIsLoaded(true),
    );
    return () => {
      isCurrent = false;
    };
  }, [connectionId, isKnown]);

  const {name} = connection;
  useEffect(() => {
    document.title = `${name} · molcajete`;
    return () => {
      document.title = 'molcajete';
    };
  }, [name]);

  return {...connection, isLoaded};
}
