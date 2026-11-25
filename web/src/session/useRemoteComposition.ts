import {useCallback, useState} from 'react';
import {readPreference, writePreference} from './storage';

// Remembered per connection in this browser: the right choice depends on the
// remote host (does it have an IME?) and on the keyboard in front of the user.
const key = (connectionId: string) => `remoteComposition.${connectionId}`;

/** Whether the remote host's IME composes text for this connection. */
export function useRemoteComposition(connectionId: string) {
  const [isRemote, setIsRemote] = useState(() => Boolean(readPreference<unknown>(key(connectionId), false)));
  const set = useCallback(
    (value: boolean) => {
      setIsRemote(value);
      writePreference(key(connectionId), value);
    },
    [connectionId],
  );
  return [isRemote, set] as const;
}
