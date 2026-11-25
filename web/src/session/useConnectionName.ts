import {useEffect, useState} from 'react';
import {api} from '../api';

/** The connection's configured name (the id until it loads); also sets the tab title. */
export function useConnectionName(connectionId: string) {
  const [name, setName] = useState(connectionId);
  useEffect(() => {
    let isCurrent = true;
    api.connections().then(
      list => {
        const connection = list.find(c => c.id === connectionId);
        if (isCurrent && connection) setName(connection.name);
      },
      () => {}, // Keep showing the id.
    );
    return () => {
      isCurrent = false;
    };
  }, [connectionId]);

  useEffect(() => {
    document.title = `${name} · molcajete`;
    return () => {
      document.title = 'molcajete';
    };
  }, [name]);

  return name;
}
