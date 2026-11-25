// Connection parameters that hold credentials. The gateway encrypts them
// at rest and never sends them back: the form only writes them.
//
// Mirrors `is_secret` in `crates/gateway/src/sensitive.rs` (a test compares
// the lists): `password`, `private-key`, `sftp-passphrase`,
// `jump-host-2-password`, … but not `jump-host-host-key` or telnet's
// `password-regex`.

/** Names that are secrets wherever they appear (`SECRET_NAMES`). */
export const SECRET_NAMES: readonly string[] = [
  'password',
  'passphrase',
  'private-key',
  'client-key',
  'client-secret',
  'secret',
  'token',
];

/** Parts of a name that make it a secret (`SECRET_PARTS`). */
export const SECRET_PARTS: readonly string[] = ['password', 'passphrase', 'private-key', 'secret'];

export function isSensitive(name: string): boolean {
  const normalized = name.trim().toLowerCase().replaceAll('_', '-');
  if (normalized.endsWith('-regex')) return false;
  return SECRET_NAMES.includes(normalized) || SECRET_PARTS.some(part => normalized.includes(part));
}
