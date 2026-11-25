// The connection form's state and its conversion from/to the API shape.
// Kept free of React so the rules can be tested directly.
import {ATTRIBUTES, type Connection} from '../api';
import {protocolInfo} from './protocols';
import {JUMP_HOST_PARAMETERS} from './jumpHosts';
import {fileTransferFields} from './fileTransfer';

export type ExtraParameter = {key: number; name: string; value: string};

export type ConnectionForm = {
  id: string;
  name: string;
  protocol: string;
  guacdHost: string;
  guacdPort: string;
  /** Seconds, '' for the gateway's default. */
  guacdHandshakeTimeout: string;
  /** Folder id, '' for the top level; only used when creating. */
  parentId: string;
  /**
   * Values of the fields the current protocol shows, of the jump host
   * fields, and new values of other stored credentials.
   */
  fields: Record<string, string>;
  /** Every other guacd parameter, edited as free name/value pairs. */
  extras: ExtraParameter[];
  recording: RecordingChoices;
  /**
   * Credentials the gateway has stored. Their values never reach the form:
   * a value typed into `fields` replaces one, and saving without keeps it.
   */
  storedSecrets: string[];
  /** Stored credentials to remove when saving. */
  clearedSecrets: string[];
};

/** What a credential field shows instead of the stored value. */
export type SecretState = 'none' | 'stored' | 'cleared';

export function secretState(form: ConnectionForm, name: string): SecretState {
  if (!form.storedSecrets.includes(name)) return 'none';
  return form.clearedSecrets.includes(name) ? 'cleared' : 'stored';
}

/** Marks a stored credential for removal, dropping anything typed into it. */
export function clearSecret(form: ConnectionForm, name: string): ConnectionForm {
  const fields = {...form.fields};
  delete fields[name];
  return {
    ...form,
    fields,
    extras: form.extras.filter(e => e.name.trim() !== name),
    clearedSecrets: [...form.clearedSecrets.filter(n => n !== name), name],
  };
}

/** Keeps a stored credential that was marked for removal. */
export function keepSecret(form: ConnectionForm, name: string): ConnectionForm {
  return {...form, clearedSecrets: form.clearedSecrets.filter(n => n !== name)};
}

/** Stored credentials without a field of their own, such as `sftp-password`. */
export function otherStoredSecrets(form: ConnectionForm): string[] {
  const known = new Set([
    ...protocolInfo(form.protocol).fields.map(f => f.name),
    ...fileTransferFields(form.protocol).map(f => f.name),
  ]);
  return form.storedSecrets.filter(name => !known.has(name) && !JUMP_HOST_PARAMETERS.has(name));
}

/** Whether and how sessions are recorded (connection attributes, not guacd parameters). */
export type RecordingChoices = {
  record: boolean;
  /** Key presses, which include every password typed. */
  keys: boolean;
  mouse: boolean;
  /** molcajeted's zstd images and video, which only molcajete's player can play. */
  molcajeteFormats: boolean;
};

const NO_RECORDING: RecordingChoices = {record: false, keys: false, mouse: true, molcajeteFormats: false};

function toRecordingChoices(attributes: Record<string, string> = {}): RecordingChoices {
  const flag = (name: string) => attributes[name] === 'true';
  return {
    record: flag(ATTRIBUTES.record),
    keys: flag(ATTRIBUTES.recordKeys),
    mouse: !flag(ATTRIBUTES.recordExcludeMouse),
    molcajeteFormats: flag(ATTRIBUTES.recordMolcajeteFormats),
  };
}

/** The attributes of the choices; the details are kept while recording is off. */
function toAttributes(r: RecordingChoices): Record<string, string> {
  const flags: [string, boolean][] = [
    [ATTRIBUTES.record, r.record],
    [ATTRIBUTES.recordKeys, r.keys],
    [ATTRIBUTES.recordExcludeMouse, !r.mouse],
    [ATTRIBUTES.recordMolcajeteFormats, r.molcajeteFormats],
  ];
  return Object.fromEntries(flags.filter(([, on]) => on).map(([name]) => [name, 'true']));
}

let nextExtraKey = 1;
export const extraParameter = (name = '', value = ''): ExtraParameter => ({key: nextExtraKey++, name, value});

/** Splits parameters into the protocol's own fields (with jump host and file transfer fields) and free extras. */
function splitParameters(protocol: string, parameters: Record<string, string>) {
  const known = new Set([
    ...protocolInfo(protocol).fields.map(f => f.name),
    ...JUMP_HOST_PARAMETERS,
    ...fileTransferFields(protocol).map(f => f.name),
  ]);
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

export function emptyConnectionForm(parentId: string | null = null): ConnectionForm {
  return {
    id: '',
    name: '',
    protocol: 'vnc',
    guacdHost: '',
    guacdPort: '',
    guacdHandshakeTimeout: '',
    parentId: parentId ?? '',
    fields: {},
    extras: [],
    recording: NO_RECORDING,
    storedSecrets: [],
    clearedSecrets: [],
  };
}

export function toConnectionForm(c: Connection): ConnectionForm {
  return {
    id: c.id,
    name: c.name,
    protocol: c.protocol,
    guacdHost: c.guacdHost ?? '',
    guacdPort: c.guacdPort == null ? '' : String(c.guacdPort),
    guacdHandshakeTimeout: c.guacdHandshakeTimeout == null ? '' : String(c.guacdHandshakeTimeout),
    parentId: c.parentId ?? '',
    ...splitParameters(c.protocol, c.parameters),
    recording: toRecordingChoices(c.attributes),
    storedSecrets: c.storedSecrets ?? [],
    clearedSecrets: [],
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

/** Empty, or whole seconds from 1 to an hour (what the gateway accepts). */
export function isHandshakeTimeoutValid(form: ConnectionForm) {
  const seconds = form.guacdHandshakeTimeout.trim();
  return seconds === '' || (/^\d{1,4}$/.test(seconds) && Number(seconds) >= 1 && Number(seconds) <= 3600);
}

export type FormResult = {ok: true; connection: Connection} | {ok: false; reason: string};

export function toConnection(form: ConnectionForm): FormResult {
  if (!form.name.trim()) return {ok: false, reason: 'Give the connection a name.'};
  if (!isGuacdPortValid(form)) return {ok: false, reason: 'The guacd port must be a number.'};
  if (!isHandshakeTimeoutValid(form)) {
    return {ok: false, reason: 'The handshake timeout must be 1 to 3600 seconds.'};
  }
  const guacdHost = form.guacdHost.trim();
  const guacdPort = form.guacdPort.trim();
  const parameters = joinParameters(form);
  // A value typed after clearing replaces the stored one anyway.
  const clearSecrets = form.clearedSecrets.filter(name => !(name in parameters));
  const handshakeTimeout = form.guacdHandshakeTimeout.trim();
  return {
    ok: true,
    connection: {
      id: form.id.trim(),
      name: form.name.trim(),
      protocol: form.protocol,
      guacdHost: guacdHost || null,
      guacdPort: guacdHost && guacdPort ? Number(guacdPort) : null,
      guacdHandshakeTimeout: handshakeTimeout ? Number(handshakeTimeout) : null,
      parameters,
      ...(clearSecrets.length > 0 ? {clearSecrets} : {}),
      ...(form.parentId ? {parentId: form.parentId} : {}),
      attributes: toAttributes(form.recording),
    },
  };
}
