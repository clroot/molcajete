// How the remote desktop follows this browser's window (RDP): its scale and
// whether it is resized to the window. A choice made in the session menu is
// remembered per connection in this browser and wins over the connection's
// own parameters, for this browser only.
import {useCallback, useState} from 'react';
import {readPreference, writePreference} from './storage';

/** A DPI, or `'auto'` to follow this screen's pixel ratio. */
export type RemoteScale = 'auto' | number;

/** The choices remembered in this browser; a missing one follows the connection. */
export type DisplayChoices = {scale?: RemoteScale; resize?: boolean};

/** The connection's own parameters, `null` where unset. */
export type ConnectionDisplay = {dpi: string | null; resizeMethod: string | null};

const scaleKey = (connectionId: string) => `remoteScale.${connectionId}`;
const resizeKey = (connectionId: string) => `resizeToWindow.${connectionId}`;

export function readDisplayChoices(connectionId: string): DisplayChoices {
  const scale = readPreference<unknown>(scaleKey(connectionId), null);
  const resize = readPreference<unknown>(resizeKey(connectionId), null);
  return {
    scale: scale === 'auto' || (typeof scale === 'number' && scale > 0) ? scale : undefined,
    resize: typeof resize === 'boolean' ? resize : undefined,
  };
}

/** Tunnel URL parameters that apply the choices from the start of a session. */
export function tunnelOverrides(choices: DisplayChoices): Record<string, string> {
  const parameters: Record<string, string> = {};
  // Empty removes the connection's value: the DPI follows the browser.
  if (choices.scale !== undefined)
    parameters['param.dpi'] = choices.scale === 'auto' ? '' : String(choices.scale);
  if (choices.resize !== undefined)
    parameters['param.resize-method'] = choices.resize ? 'display-update' : '';
  return parameters;
}

/** What is in effect: this browser's choice, else the connection's value. */
export function effectiveDisplay(choices: DisplayChoices, connection: ConnectionDisplay) {
  const dpi = Number(connection.dpi);
  const scale: RemoteScale = choices.scale ?? (Number.isInteger(dpi) && dpi > 0 ? dpi : 'auto');
  const resize = choices.resize ?? connection.resizeMethod === 'display-update';
  return {scale, resize};
}

/** The display choices of a connection in this browser. */
export function useDisplayChoices(connectionId: string) {
  const [choices, setChoices] = useState(() => readDisplayChoices(connectionId));
  const setScale = useCallback(
    (scale: RemoteScale) => {
      writePreference(scaleKey(connectionId), scale);
      setChoices(c => ({...c, scale}));
    },
    [connectionId],
  );
  const setResize = useCallback(
    (resize: boolean) => {
      writePreference(resizeKey(connectionId), resize);
      setChoices(c => ({...c, resize}));
    },
    [connectionId],
  );
  return {choices, setScale, setResize};
}
