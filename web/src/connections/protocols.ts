// Connection form fields per guacd protocol. Parameters not listed here
// are edited as free key/value pairs.
import {AUTOMATIC_SCALE_LABEL, SCALE_OPTIONS} from '../session/displayScale';

export type FieldKind = 'text' | 'password' | 'number' | 'multiline' | 'switch' | 'select';

export type ParameterField = {
  name: string;
  label: string;
  kind: FieldKind;
  description?: string;
  placeholder?: string;
  isRequired?: boolean;
  options?: {value: string; label: string}[];
  /** The value of a switch that is on; guacd's `true` by default. */
  onValue?: string;
  /** Set when a new connection takes this protocol. */
  defaultValue?: string;
  /** Shown under *Clipboard and input* instead of with the protocol's settings. */
  section?: 'input';
};

export type Protocol = {
  value: string;
  label: string;
  fields: ParameterField[];
};

const hostname: ParameterField = {
  name: 'hostname',
  label: 'Hostname',
  kind: 'text',
  placeholder: '192.168.0.10',
  isRequired: true,
};
/** guacd uses the protocol's standard port when none is given. */
const port = (defaultPort: number): ParameterField => ({
  name: 'port',
  label: 'Port',
  kind: 'number',
  placeholder: String(defaultPort),
});
const username: ParameterField = {name: 'username', label: 'Username', kind: 'text'};
const password: ParameterField = {
  name: 'password',
  label: 'Password',
  kind: 'password',
  description: 'Leave it empty to type it in when connecting instead of storing it.',
};

/** What users may do in a session; every protocol has these. */
const INPUT_RESTRICTIONS: ParameterField[] = [
  {
    name: 'read-only',
    label: 'Read-only',
    kind: 'switch',
    description: 'Users see the session, but their keys, clicks and touches are not sent.',
    section: 'input',
  },
  {
    name: 'disable-copy',
    label: 'Block copying from the remote side',
    kind: 'switch',
    description: "Text copied on the remote computer does not reach the user's clipboard.",
    section: 'input',
  },
  {
    name: 'disable-paste',
    label: 'Block pasting into the remote side',
    kind: 'switch',
    description: "The user's clipboard is not sent to the remote computer.",
    section: 'input',
  },
];

const withInputRestrictions = (protocol: Protocol): Protocol => ({
  ...protocol,
  fields: [...protocol.fields, ...INPUT_RESTRICTIONS],
});

const PROTOCOL_SETTINGS: Protocol[] = [
  {
    value: 'vnc',
    label: 'VNC',
    fields: [
      hostname,
      port(5900),
      {...username, description: 'Only for servers that ask for one.'},
      password,
    ],
  },
  {
    value: 'rdp',
    label: 'RDP',
    fields: [
      hostname,
      port(3389),
      username,
      password,
      {name: 'domain', label: 'Domain', kind: 'text'},
      {
        name: 'security',
        label: 'Security mode',
        kind: 'select',
        options: [
          {value: 'any', label: 'Negotiate (any)'},
          {value: 'nla', label: 'NLA'},
          {value: 'nla-ext', label: 'Extended NLA'},
          {value: 'tls', label: 'TLS'},
          {value: 'rdp', label: 'RDP encryption'},
          {value: 'vmconnect', label: 'Hyper-V / VMConnect'},
        ],
      },
      {
        name: 'ignore-cert',
        label: 'Ignore server certificate',
        kind: 'switch',
        description: 'Accept self-signed certificates.',
      },
      {
        name: 'server-layout',
        label: 'Keyboard layout',
        kind: 'select',
        description:
          'The layout of the remote session. Korean lets the remote computer compose Hangul and enables its 한/영 and 한자 keys.',
        options: [
          {value: 'en-us-qwerty', label: 'English (US)'},
          {value: 'ko-kr-qwerty', label: 'Korean'},
          {value: 'failsafe', label: 'Any (send text as Unicode)'},
        ],
      },
      {
        name: 'resize-method',
        label: 'Resize the remote desktop to the window',
        kind: 'switch',
        description: 'The remote desktop takes the size of the browser window and follows it.',
        onValue: 'display-update',
        defaultValue: 'display-update',
      },
      {
        name: 'dpi',
        label: 'Display scale',
        kind: 'select',
        description: 'Size of text and apps on the remote desktop.',
        placeholder: AUTOMATIC_SCALE_LABEL,
        options: SCALE_OPTIONS,
      },
    ],
  },
  {
    value: 'ssh',
    label: 'SSH',
    fields: [
      hostname,
      port(22),
      username,
      password,
      {
        name: 'private-key',
        label: 'Private key',
        kind: 'multiline',
        placeholder: '-----BEGIN OPENSSH PRIVATE KEY-----',
      },
      {name: 'passphrase', label: 'Key passphrase', kind: 'password'},
      {name: 'font-size', label: 'Font size', kind: 'number'},
    ],
  },
  {
    value: 'telnet',
    label: 'Telnet',
    fields: [hostname, port(23), username, password, {name: 'font-size', label: 'Font size', kind: 'number'}],
  },
  {
    value: 'kubernetes',
    label: 'Kubernetes',
    fields: [
      hostname,
      port(8080),
      {name: 'namespace', label: 'Namespace', kind: 'text', placeholder: 'default'},
      {name: 'pod', label: 'Pod', kind: 'text'},
      {name: 'container', label: 'Container', kind: 'text'},
      {name: 'use-ssl', label: 'Use TLS', kind: 'switch'},
      {name: 'ignore-cert', label: 'Ignore server certificate', kind: 'switch'},
    ],
  },
];

export const PROTOCOLS: Protocol[] = PROTOCOL_SETTINGS.map(withInputRestrictions);

export function protocolInfo(value: string): Protocol {
  return PROTOCOLS.find(p => p.value === value) ?? PROTOCOLS[0];
}

export function protocolLabel(value: string): string {
  return PROTOCOLS.find(p => p.value === value)?.label ?? value.toUpperCase();
}
