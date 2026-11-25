// Connection form fields per guacd protocol. Parameters not listed here
// are edited as free key/value pairs.

export type FieldKind = 'text' | 'password' | 'number' | 'multiline' | 'switch' | 'select';

export type ParameterField = {
  name: string;
  label: string;
  kind: FieldKind;
  description?: string;
  placeholder?: string;
  options?: {value: string; label: string}[];
};

export type Protocol = {
  value: string;
  label: string;
  defaultPort: string;
  fields: ParameterField[];
};

const hostname: ParameterField = {name: 'hostname', label: 'Hostname', kind: 'text', placeholder: '192.168.0.10'};
const port: ParameterField = {name: 'port', label: 'Port', kind: 'number'};
const username: ParameterField = {name: 'username', label: 'Username', kind: 'text'};
const password: ParameterField = {name: 'password', label: 'Password', kind: 'password'};

export const PROTOCOLS: Protocol[] = [
  {
    value: 'vnc',
    label: 'VNC',
    defaultPort: '5900',
    fields: [hostname, port, {...username, description: 'Only for servers that ask for one.'}, password],
  },
  {
    value: 'rdp',
    label: 'RDP',
    defaultPort: '3389',
    fields: [
      hostname,
      port,
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
    defaultPort: '22',
    fields: [
      hostname,
      port,
      username,
      password,
      {name: 'private-key', label: 'Private key', kind: 'multiline', placeholder: '-----BEGIN OPENSSH PRIVATE KEY-----'},
      {name: 'passphrase', label: 'Key passphrase', kind: 'password'},
      {name: 'font-size', label: 'Font size', kind: 'number'},
    ],
  },
  {
    value: 'telnet',
    label: 'Telnet',
    defaultPort: '23',
    fields: [hostname, port, username, password, {name: 'font-size', label: 'Font size', kind: 'number'}],
  },
  {
    value: 'kubernetes',
    label: 'Kubernetes',
    defaultPort: '8080',
    fields: [
      hostname,
      port,
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
