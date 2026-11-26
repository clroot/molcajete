// How the remote desktop follows this browser's window (RDP): its scale and
// whether it is resized to the window; and the text size of a terminal. A
// choice made in the session menu is remembered per connection in this
// browser and wins over the connection's own parameters, for this browser
// only.
import {useCallback, useState} from 'react';
import {readPreference, writePreference} from './storage';

/** A DPI, or `'auto'` to follow this screen's pixel ratio. */
export type RemoteScale = 'auto' | number;

/** Text sizes of a terminal, in points: what the session menu offers, and guacd's default. */
export const TEXT_SIZE = {min: 6, max: 18, standard: 12} as const;

/** The nearest size the session menu offers. */
export const clampTextSize = (size: number) =>
  Math.min(TEXT_SIZE.max, Math.max(TEXT_SIZE.min, Math.round(size)));

/** The choices remembered in this browser; a missing one follows the connection. */
export type DisplayChoices = {scale?: RemoteScale; resize?: boolean; textSize?: number};

/** The connection's own parameters, `null` where unset. Older gateways do not send `fontSize`. */
export type ConnectionDisplay = {dpi: string | null; resizeMethod: string | null; fontSize?: string | null};

const scaleKey = (connectionId: string) => `remoteScale.${connectionId}`;
const resizeKey = (connectionId: string) => `resizeToWindow.${connectionId}`;
const textSizeKey = (connectionId: string) => `textSize.${connectionId}`;

export function readDisplayChoices(connectionId: string): DisplayChoices {
  const scale = readPreference<unknown>(scaleKey(connectionId), null);
  const resize = readPreference<unknown>(resizeKey(connectionId), null);
  const textSize = readPreference<unknown>(textSizeKey(connectionId), null);
  return {
    scale: scale === 'auto' || (typeof scale === 'number' && scale > 0) ? scale : undefined,
    resize: typeof resize === 'boolean' ? resize : undefined,
    textSize: typeof textSize === 'number' && textSize > 0 ? clampTextSize(textSize) : undefined,
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
  if (choices.textSize !== undefined) parameters['param.font-size'] = String(choices.textSize);
  return parameters;
}

/** What is in effect: this browser's choice, else the connection's value. */
export function effectiveDisplay(choices: DisplayChoices, connection: ConnectionDisplay) {
  const dpi = Number(connection.dpi);
  const scale: RemoteScale = choices.scale ?? (Number.isInteger(dpi) && dpi > 0 ? dpi : 'auto');
  const resize = choices.resize ?? connection.resizeMethod === 'display-update';
  const fontSize = Number(connection.fontSize);
  const textSize = choices.textSize ?? (fontSize > 0 ? fontSize : TEXT_SIZE.standard);
  return {scale, resize, textSize};
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
  const setTextSize = useCallback(
    (textSize: number) => {
      writePreference(textSizeKey(connectionId), textSize);
      setChoices(c => ({...c, textSize}));
    },
    [connectionId],
  );
  return {choices, setScale, setResize, setTextSize};
}
