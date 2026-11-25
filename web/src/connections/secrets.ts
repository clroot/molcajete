// Connection parameters that hold credentials. The gateway encrypts them
// at rest and never sends them back: the form only writes them.

/**
 * Same list as `SENSITIVE_PARAMETERS` in `crates/gateway/src/secrets.rs`
 * (a test compares them). Every protocol field of kind `password` must be
 * on it.
 */
export const SENSITIVE_PARAMETERS: readonly string[] = [
  'password',
  'passphrase',
  'private-key',
  'sftp-password',
  'sftp-passphrase',
  'sftp-private-key',
  'gateway-password',
  'client-key',
  // SSH jump hosts: the single hop, and hops 1 to 4 of a chain.
  ...['jump-host', 'jump-host-1', 'jump-host-2', 'jump-host-3', 'jump-host-4'].flatMap(prefix => [
    `${prefix}-password`,
    `${prefix}-private-key`,
    `${prefix}-passphrase`,
  ]),
];

export function isSensitive(name: string): boolean {
  return SENSITIVE_PARAMETERS.includes(name.trim());
}
