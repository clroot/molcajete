import {useCallback, useState} from 'react';

// Remembered per connection in this browser: the right choice depends on the
// remote host (does it have an IME?) and on the keyboard in front of the user.
const storageKey = (connectionId: string) => `molcajete.remoteComposition.${connectionId}`;

function read(connectionId: string): boolean {
  try {
    return localStorage.getItem(storageKey(connectionId)) === '1';
  } catch {
    return false; // Storage blocked: the choice lasts for this page only.
  }
}

/** Whether the remote host's IME composes text for this connection. */
export function useRemoteComposition(connectionId: string) {
  const [isRemote, setIsRemote] = useState(() => read(connectionId));
  const set = useCallback(
    (value: boolean) => {
      setIsRemote(value);
      try {
        if (value) localStorage.setItem(storageKey(connectionId), '1');
        else localStorage.removeItem(storageKey(connectionId));
      } catch {
        // Not remembered.
      }
    },
    [connectionId],
  );
  return [isRemote, set] as const;
}
