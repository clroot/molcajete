// SSH jump hosts: molcajeted parameters every protocol takes (guacd
// ignores them). Per-hop values (jump-host-2-password, ...) are edited as
// other parameters under Advanced.
import type {ParameterField} from './protocols';

export const JUMP_HOST_FIELDS: ParameterField[] = [
  {
    name: 'jump-hosts',
    label: 'Jump hosts',
    kind: 'multiline',
    description:
      'One per line, in the order they are passed through: user@host or user@host:port. Only molcajeted connects through them.',
    placeholder: 'admin@bastion.example.com:22',
  },
  {
    name: 'jump-host-username',
    label: 'Username',
    kind: 'text',
    description: 'For jump hosts listed without one.',
  },
  {name: 'jump-host-password', label: 'Password', kind: 'password'},
  {
    name: 'jump-host-private-key',
    label: 'Private key',
    kind: 'multiline',
    placeholder: '-----BEGIN OPENSSH PRIVATE KEY-----',
  },
  {name: 'jump-host-passphrase', label: 'Key passphrase', kind: 'password'},
  {
    name: 'jump-host-host-key',
    label: 'Host key',
    kind: 'multiline',
    description:
      'Connect only if the jump host presents this key: a known_hosts line, a public key or a SHA256 fingerprint. Without it any key is accepted.',
    placeholder: 'ssh-ed25519 AAAA…',
  },
];

export const JUMP_HOST_PARAMETERS = new Set(JUMP_HOST_FIELDS.map(f => f.name));

/** A readable label for a parameter the daemon asks for (`required`). */
export function requiredLabel(parameter: string): string {
  const shared: Record<string, string> = {
    'jump-host-username': 'Jump host username',
    'jump-host-password': 'Jump host password',
  };
  const hop = /^jump-host-(\d+)-password$/.exec(parameter);
  if (hop) return `Jump host ${hop[1]} password`;
  return shared[parameter] ?? parameter;
}
