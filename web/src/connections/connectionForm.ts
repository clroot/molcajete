// The connection form's state and its conversion from/to the API shape.
// Kept free of React so the rules can be tested directly.
import type {Connection} from '../api';
import {protocolInfo} from './protocols';

export type ExtraParameter = {key: number; name: string; value: string};

export type ConnectionForm = {
  id: string;
  name: string;
  protocol: string;
  guacdHost: string;
  guacdPort: string;
  /** Values of the fields the current protocol shows. */
  fields: Record<string, string>;
  /** Every other guacd parameter, edited as free name/value pairs. */
  extras: ExtraParameter[];
};

let nextExtraKey = 1;
export const extraParameter = (name = '', value = ''): ExtraParameter => ({key: nextExtraKey++, name, value});

/** Splits parameters into the protocol's own fields and free extras. */
function splitParameters(protocol: string, parameters: Record<string, string>) {
  const known = new Set(protocolInfo(protocol).fields.map(f => f.name));
  const fields: Record<string, string> = {};
  const extras: ExtraParameter[] = [];
  for (const [name, value] of Object.entries(parameters)) {
    if (known.has(name)) fields[name] = value;
    else extras.push(extraParameter(name, value));
  }
  return {fields, extras};
}

/** All parameters with a value; protocol fields win over extras of the same name. */
function joinParameters(form: ConnectionForm): Record<string, string> {
  const parameters: Record<string, string> = {};
  for (const {name, value} of form.extras) {
    if (name.trim() && value !== '') parameters[name.trim()] = value;
  }
  for (const [name, value] of Object.entries(form.fields)) {
    if (value !== '') parameters[name] = value;
  }
  return parameters;
}

export function emptyConnectionForm(): ConnectionForm {
  return {id: '', name: '', protocol: 'vnc', guacdHost: '', guacdPort: '', fields: {}, extras: []};
}

export function toConnectionForm(c: Connection): ConnectionForm {
  return {
    id: c.id,
    name: c.name,
    protocol: c.protocol,
    guacdHost: c.guacdHost ?? '',
    guacdPort: c.guacdPort == null ? '' : String(c.guacdPort),
    ...splitParameters(c.protocol, c.parameters),
  };
}

/**
 * Switches protocol without losing anything: values the new protocol has
 * no field for move to the extras, and extras it does have a field for
 * move into that field. A new connection also gets the protocol's
 * defaults for fields that have no value.
 */
export function changeProtocol(form: ConnectionForm, protocol: string, isNew = false): ConnectionForm {
  const changed = {...form, protocol, ...splitParameters(protocol, joinParameters(form))};
  if (isNew) {
    for (const field of protocolInfo(protocol).fields) {
      if (field.defaultValue !== undefined && !changed.fields[field.name]) {
        changed.fields[field.name] = field.defaultValue;
      }
    }
  }
  return changed;
}

/** A URL-safe id derived from the name, suggested for new connections. */
export function slugify(name: string) {
  return name
    .toLowerCase()
    .normalize('NFKD')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64);
}

export function isGuacdPortValid(form: ConnectionForm) {
  const port = form.guacdPort.trim();
  return port === '' || /^\d{1,5}$/.test(port);
}

export type FormResult = {ok: true; connection: Connection} | {ok: false; reason: string};

export function toConnection(form: ConnectionForm): FormResult {
  if (!form.name.trim()) return {ok: false, reason: 'Give the connection a name.'};
  if (!isGuacdPortValid(form)) return {ok: false, reason: 'The guacd port must be a number.'};
  const guacdHost = form.guacdHost.trim();
  const guacdPort = form.guacdPort.trim();
  return {
    ok: true,
    connection: {
      id: form.id.trim(),
      name: form.name.trim(),
      protocol: form.protocol,
      guacdHost: guacdHost || null,
      guacdPort: guacdHost && guacdPort ? Number(guacdPort) : null,
      parameters: joinParameters(form),
    },
  };
}
