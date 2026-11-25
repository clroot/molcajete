// Connection form fields per guacd protocol. Parameters not listed here
// are edited as free key/value pairs.

export type FieldKind = 'text' | 'password' | 'number' | 'multiline' | 'switch' | 'select';

export type ParameterField = {
  name: string;
  label: string;
  kind: FieldKind;
  description?: string;
  placeholder?: string;
  isRequired?: boolean;
  options?: {value: string; label: string}[];
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
const password: ParameterField = {name: 'password', label: 'Password', kind: 'password'};

export const PROTOCOLS: Protocol[] = [
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

export function protocolInfo(value: string): Protocol {
  return PROTOCOLS.find(p => p.value === value) ?? PROTOCOLS[0];
}

export function protocolLabel(value: string): string {
  return PROTOCOLS.find(p => p.value === value)?.label ?? value.toUpperCase();
}
